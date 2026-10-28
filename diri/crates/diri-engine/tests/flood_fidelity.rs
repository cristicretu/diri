//! A flood through the real Holder and Engine parses every byte, in order.
//!
//! A program prints far more than the session log keeps (progress-bar rows
//! rewritten many times each, styled, linked, wide and emoji text, titles,
//! modes, an inline image, and a visit to the alternate screen at the end). The Engine's
//! terminal afterwards must equal a one-shot parse of the same bytes: every
//! retained history row with its cells and metadata, the visible screen,
//! title and modes. A read loop that skips, coalesces or restarts mid-stream
//! loses rows or state changes and fails here.
//!
//! Run once with nobody attached, and once with an attached client that
//! reads nothing until the flood is over: the Engine's own parse must not
//! depend on its clients, and the client must then recover the final screen.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use diri_engine::control::ControlServer;
use diri_engine::session::HolderConfig;
use diri_engine::{ManifestEngine, Registry};
use diri_proto::ControlMessage;
use diri_proto::frames::{FrameCodec, FrameType};
use diri_terminal_state::HeadlessScreen;
use serde_json::json;

const COLS: usize = 160;
const ROWS: usize = 50;
/// Three times the session log's 8 MiB on-disk window by default;
/// `DIRI_FLOOD_MB` floods more.
fn flood_bytes() -> usize {
    std::env::var("DIRI_FLOOD_MB")
        .ok()
        .and_then(|mb| mb.parse::<usize>().ok())
        .unwrap_or(24)
        << 20
}

/// History retains 10,000 rows; the flood stays under that so a lost row
/// anywhere in it shows.
const FLOOD_ROWS: usize = 6_000;
const END: &str = "FLOOD-END";

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            out.push(if i <= chunk.len() {
                TABLE[(n >> (18 - 6 * i)) as usize & 63] as char
            } else {
                '='
            });
        }
    }
    out
}

/// Rows rewritten many times each (a progress bar), so the history keeps
/// every one of them while the bytes outrun the log many times over.
fn flood() -> Vec<u8> {
    let total = flood_bytes();
    // About 200 bytes per pass.
    let passes = (total / FLOOD_ROWS / 200).max(4);
    let pixels: Vec<u8> = (0..4 * 4 * 3).map(|i| (i * 5) as u8).collect();
    let mut out = Vec::with_capacity(total + (64 << 10));
    out.extend_from_slice(
        format!(
            "\x1b_Ga=T,f=24,s=4,v=4,i=3,q=2;{}\x1b\\\r\n",
            base64(&pixels)
        )
        .as_bytes(),
    );
    let mut row = 0usize;
    while out.len() < total {
        for pass in 0..passes {
            // Wide text and truecolor: cheap to pass on, costly to parse.
            out.extend_from_slice(
                format!(
                    "\r\x1b[38;5;{}mrow {row:06} pass {pass:04} \x1b[48;2;{};{};{}m{}\x1b[0m 日本語のテキスト 你好世界",
                    (row + pass) % 256,
                    row % 256,
                    pass % 256,
                    (row * 7) % 256,
                    "=".repeat(60 + (row + pass) % 40),
                )
                .as_bytes(),
            );
        }
        match row % 10 {
            0 => out.extend_from_slice(format!("\x1b]2;title {row}\x07").as_bytes()),
            1 => out.extend_from_slice(
                format!(" \x1b]8;id=r{row};https://example.invalid/{row}\x07link\x1b]8;;\x07")
                    .as_bytes(),
            ),
            2 => out.extend_from_slice(" 漢字 👩\u{200d}💻 🇷🇴 e\u{301}".as_bytes()),
            3 => out.extend_from_slice(b"\x1b[?2004h\x1b[?1000h\x1b[?1006h\x1b[?1000l"),
            4 => out.extend_from_slice(b" \x1b[4:3;58;5;196mcurly\x1b[24;59m"),
            5 if row % 500 == 5 => out.extend_from_slice(b" \x1b_Ga=p,i=3,q=2\x1b\\"),
            _ => {}
        }
        out.extend_from_slice(b"\r\n");
        row += 1;
    }
    out.extend_from_slice(
        format!(
            "last row {row}\r\n\x1b]2;final title\x07\x1b[?1003h\x1b[?1049h\x1b[2;3Halternate\x1b[?1049l\x1b[1;35m{END}\x1b[0m"
        )
        .as_bytes(),
    );
    out
}

struct Daemon {
    server: Arc<ControlServer>,
    registry: Arc<Mutex<Registry>>,
    control: UnixStream,
    replies: BufReader<UnixStream>,
    next_id: u64,
    _temp: tempfile::TempDir,
}

impl Daemon {
    fn start() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let (engine, _) =
            ManifestEngine::load_dir(&diri_engine::detect::bundled_manifest_dir()).unwrap();
        let registry = Arc::new(Mutex::new(Registry::new(
            Arc::new(engine),
            temp.path().join("state.json"),
        )));
        let server = Arc::new(
            ControlServer::new(Arc::clone(&registry), temp.path().join("daemon.sock"))
                .with_logs_dir(temp.path().join("logs"))
                .with_holder(HolderConfig {
                    holders_dir: temp.path().join("holders"),
                    executable: env!("CARGO_BIN_EXE_diri-holder").into(),
                }),
        );
        let listener = server.bind().unwrap();
        let serving = Arc::clone(&server);
        std::thread::spawn(move || {
            while let Ok((stream, _)) = listener.accept() {
                let server = Arc::clone(&serving);
                std::thread::spawn(move || {
                    let _ = server.serve(stream);
                });
            }
        });
        let control = UnixStream::connect(server.socket_path()).unwrap();
        control
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        let replies = BufReader::new(control.try_clone().unwrap());
        Self {
            server,
            registry,
            control,
            replies,
            next_id: 1,
            _temp: temp,
        }
    }

    fn request(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let message = ControlMessage::Request {
            id: self.next_id,
            method: method.into(),
            params: Some(params),
        };
        self.next_id += 1;
        let mut bytes = serde_json::to_vec(&message).unwrap();
        bytes.push(b'\n');
        self.control.write_all(&bytes).unwrap();
        let mut line = String::new();
        self.replies.read_line(&mut line).unwrap();
        match serde_json::from_str::<ControlMessage>(&line).unwrap() {
            ControlMessage::Response {
                result: Ok(value), ..
            } => value,
            other => panic!("{method} failed: {other:?}"),
        }
    }
}

/// What the terminal holds, read the way a client reads it.
#[derive(Debug, PartialEq)]
struct Terminal {
    lines: Vec<String>,
    visible_start_row: i64,
    alt_screen: bool,
    pages: Vec<(Vec<u8>, String)>,
    title: Option<String>,
    modes: String,
}

/// Image anchors name a per-screen store; only the rest must match.
fn without_store_nonce(text: String) -> String {
    const ANCHOR: &str = "diri-image:";
    let mut out = String::with_capacity(text.len());
    let mut rest = text.as_str();
    while let Some(at) = rest.find(ANCHOR) {
        out.push_str(&rest[..at + ANCHOR.len()]);
        rest = &rest[at + ANCHOR.len()..];
        out.push('#');
        rest = &rest[rest.find('.').unwrap_or(0)..];
    }
    out.push_str(rest);
    out
}

const PAGE_ROWS: i64 = 500;

fn reference(bytes: &[u8]) -> Terminal {
    // The local Engine's emulator configuration.
    let mut screen = HeadlessScreen::new_with_keyboard_enhancements(COLS, ROWS)
        .with_notifications()
        .with_inline_images()
        .with_host_replies("reference");
    screen.feed(bytes);
    let scrollback = screen.scrollback();
    let total = scrollback.visible_start_row + ROWS as i64;
    let pages = (0..total)
        .step_by(PAGE_ROWS as usize)
        .map(|first| {
            let page = screen.scrollback_cells(first, PAGE_ROWS);
            (
                page.payload,
                without_store_nonce(format!("{:?}", page.metadata)),
            )
        })
        .collect();
    Terminal {
        lines: scrollback.lines,
        visible_start_row: scrollback.visible_start_row,
        alt_screen: scrollback.is_alt_screen,
        pages,
        title: screen.title().map(str::to_owned),
        modes: format!(
            "{:?}",
            (
                screen.is_alt_screen(),
                screen.bracketed_paste(),
                screen.mouse_modes()
            )
        ),
    }
}

fn engine_terminal(registry: &Mutex<Registry>, id: &str) -> Terminal {
    let registry = registry.lock().unwrap();
    let session = registry.get(id).expect("session");
    let scrollback = session.read_scrollback();
    let total = scrollback.visible_start_row + ROWS as i64;
    let pages = (0..total)
        .step_by(PAGE_ROWS as usize)
        .map(|first| {
            let page = session.read_scrollback_cells(first, PAGE_ROWS);
            (
                page.payload,
                without_store_nonce(format!("{:?}", page.metadata)),
            )
        })
        .collect();
    Terminal {
        lines: scrollback.lines,
        visible_start_row: scrollback.visible_start_row,
        alt_screen: scrollback.is_alt_screen,
        pages,
        title: session.terminal_title().unwrap(),
        modes: format!("{:?}", session.modes()),
    }
}

fn first_difference(expected: &Terminal, actual: &Terminal) -> String {
    if expected.lines != actual.lines {
        let row = expected
            .lines
            .iter()
            .zip(&actual.lines)
            .position(|(a, b)| a != b)
            .unwrap_or(expected.lines.len().min(actual.lines.len()));
        return format!(
            "history row {row} of {} (Engine has {}): expected {:?}, Engine {:?}",
            expected.lines.len(),
            actual.lines.len(),
            expected.lines.get(row),
            actual.lines.get(row)
        );
    }
    if expected.pages != actual.pages {
        let page = expected
            .pages
            .iter()
            .zip(&actual.pages)
            .position(|(a, b)| a != b)
            .unwrap_or(0);
        return format!(
            "cells or metadata differ in rows {}..",
            page as i64 * PAGE_ROWS
        );
    }
    format!(
        "title {:?} vs {:?}, modes {} vs {}",
        expected.title, actual.title, expected.modes, actual.modes
    )
}

fn run(slow_attach: bool) {
    let stream = flood();
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("flood.bin");
    std::fs::write(&path, &stream).unwrap();
    let expected = reference(&stream);
    assert!(
        expected
            .lines
            .iter()
            .any(|line| line.starts_with("row 000000 pass")),
        "every row is retained by the reference, the first included"
    );

    let mut daemon = Daemon::start();
    // Raw output so the bytes reach the terminal exactly as written. The
    // flood starts at once, before the Engine has caught up with the new
    // session: the hardest start for a reader that must not fall behind.
    let script = format!("stty raw -echo; cat '{}'; exec sleep 600", path.display());
    let spawned = daemon.request(
        "session.spawn",
        json!({
            "kind": { "shell": {} }, "cwd": "/tmp",
            "argv": ["/bin/sh", "-c", script],
            "initialCols": COLS, "initialRows": ROWS,
        }),
    );
    let id = spawned["id"].as_str().unwrap().to_owned();

    let attached = slow_attach.then(|| {
        let mut data = UnixStream::connect(daemon.server.socket_path()).unwrap();
        writeln!(data, "{}", json!({ "attach": id })).unwrap();
        data
    });
    let started = Instant::now();
    let deadline = started + Duration::from_secs(240);
    loop {
        let done = daemon
            .registry
            .lock()
            .unwrap()
            .get(&id)
            .is_some_and(|session| session.screen_lines().iter().any(|l| l.contains(END)));
        if done {
            break;
        }
        assert!(Instant::now() < deadline, "the flood never finished");
        std::thread::sleep(Duration::from_millis(20));
    }
    eprintln!(
        "{} MB through Holder and Engine in {:?} (slow attach: {slow_attach})",
        stream.len() >> 20,
        started.elapsed()
    );
    let actual = engine_terminal(&daemon.registry, &id);
    assert!(
        expected == actual,
        "the Engine's terminal differs from a one-shot parse: {}",
        first_difference(&expected, &actual)
    );

    if let Some(mut data) = attached {
        // The client that read nothing recovers the final screen.
        data.set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        let mut codec = FrameCodec::new();
        let mut chunk = vec![0; 1 << 20];
        let mut seen = false;
        while !seen {
            let count = data.read(&mut chunk).expect("attach stream");
            assert!(count > 0, "attachment closed");
            for frame in codec.feed(&chunk[..count]).unwrap() {
                if frame.frame_type != FrameType::Grid {
                    continue;
                }
                let update = frame.grid_payload().unwrap().unwrap();
                seen |= update.changed_rows.iter().any(|row| {
                    row.cells
                        .iter()
                        .map(|c| char::from_u32(c.scalar).unwrap_or(' '))
                        .collect::<String>()
                        .contains(END)
                });
            }
        }
    }
    daemon.request("session.kill", json!({ "sessionID": id }));
}

#[test]
fn a_flood_with_nobody_attached_parses_every_byte() {
    run(false);
}

#[test]
fn a_flood_with_a_client_that_reads_nothing_parses_every_byte() {
    run(true);
}
