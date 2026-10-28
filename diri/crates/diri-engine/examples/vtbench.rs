//! The six `kitten __benchmark__` workloads through the real Holder and
//! Engine: a held session at 120x80 `cat`s each payload (raw tty, so the
//! bytes arrive as written) and the clock stops when the Engine has fed the
//! parser every byte the program wrote, the marker printed after the payload
//! included.
//!
//! Every run is checked before it reports a number:
//! - bytes the Engine handed the parser == bytes the program wrote;
//! - the Engine's terminal equals a reference parse of the same stream fed
//!   one byte at a time into the Engine's emulator configuration: every
//!   history row (text, cells, metadata), the visible grid, cursor, modes,
//!   title, hyperlinks, keyboard state, progress and stored image bytes.
//!
//! A run that fails either check panics instead of printing. History is
//! bounded by stored bytes, so how many rows fit can differ with the read
//! sizes; every row both parses still hold must match, and a difference in
//! depth is printed.
//!
//! Two more columns come from the same rounds, interleaved, so the machine's
//! load lands on all three: `parse+grid` feeds the same stream to the
//! Engine's emulator configuration in 64 KiB reads in this process, and `PTY
//! drain` times a bare reader of the same program's PTY that parses nothing,
//! the ceiling the PTY puts on any terminal that reads it. `e2e/min` is
//! Holder->Engine over the lower of the two.
//!
//! ```sh
//! DIRI_VT_BENCH_WRITE=/tmp/vt cargo bench -p diri-terminal-state --bench vt_workloads
//! cargo build --release -p diri-engine --bin diri-holder --example vtbench
//! DIRI_HOLDER_BIN=target/release/diri-holder target/release/examples/vtbench /tmp/vt
//! ```
//!
//! `DIRI_VT_BENCH_RUNS` sets the run count (default 5) and
//! `DIRI_VT_BENCH_ONLY=ascii,csi` limits the workloads.
//! `DIRI_VT_BENCH_PROFILE=1` skips the reference parse and the state check
//! so a profiler sees only the timed path; it prints no numbers.
//!
//! Reported: MB/s (10^6 bytes) over the time from spawn until the last byte
//! is parsed (PTY drain: read), minus the same for an empty payload; median
//! and range of the runs, each at least 64 MB.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use diri_engine::screen::HeadlessScreen;
use diri_engine::session::{HolderConfig, Session, SessionSpec};
use diri_engine::{Authority, ManifestEngine, Pty, PtySpec};

const MARKER: &str = "VTBENCH-END";
const MIN_BYTES: usize = 64 << 20;
const COLS: usize = 120;
const ROWS: usize = 80;

/// What the shell prints after the payload: leave the alternate screen,
/// clear, and print the marker.
fn suffix() -> Vec<u8> {
    format!("\x1b[0m\x1b[?1049l\x1b[2J\x1b[H{MARKER}").into_bytes()
}

fn engine() -> Arc<ManifestEngine> {
    let dir = diri_engine::detect::bundled_manifest_dir()
        .canonicalize()
        .expect("manifests");
    let (engine, _) = ManifestEngine::load_dir(&dir).expect("load");
    Arc::new(engine)
}

/// Everything about a terminal that a dropped, reordered or misparsed byte
/// could change, except what the Engine consumes as it goes (replies,
/// notifications, clipboard).
#[derive(PartialEq, Debug)]
struct Observed {
    scrollback_lines: Vec<String>,
    visible_start_row: i64,
    cells_payload: Vec<u8>,
    cells_metadata: String,
    full_snapshot: String,
    link_text: String,
    links: Vec<(usize, String)>,
    title: Option<String>,
    modes: String,
    cursor: (u16, u16, bool),
    keyboard: String,
    progress: Option<(i64, i64)>,
    image_bytes: usize,
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

/// Rows of history `screen` retains.
fn history_rows(screen: &mut HeadlessScreen) -> usize {
    screen.scrollback().visible_start_row as usize
}

/// The terminal with its newest `history` rows of scrollback.
///
/// History is bounded by stored bytes, and how many rows fit depends on how
/// they were compressed, which follows the read sizes; so the depth may
/// differ between two parses of the same bytes. Every row both still hold
/// must match, cell for cell.
fn observe(screen: &mut HeadlessScreen, history: usize) -> Observed {
    let scrollback = screen.scrollback();
    let retained = scrollback.visible_start_row as usize;
    let first = retained - history.min(retained);
    let mut lines = scrollback.lines;
    let keep = (retained - first + ROWS).min(lines.len());
    lines.drain(..lines.len() - keep);
    let cells = screen.scrollback_cells(first as i64, (retained - first + ROWS) as i64);
    let links = screen.link_source(retained - first);
    Observed {
        scrollback_lines: lines,
        visible_start_row: (retained - first) as i64,
        cells_payload: cells.payload,
        cells_metadata: without_store_nonce(format!("{:?}", cells.metadata)),
        full_snapshot: without_store_nonce(format!("{:?}", screen.full_snapshot())),
        link_text: links.text,
        links: links
            .hyperlinks
            .into_iter()
            .map(|(at, uri)| (at, without_store_nonce(uri)))
            .collect(),
        title: screen.title().map(str::to_owned),
        modes: format!(
            "alt={} paste={} focus={} mouse={:?} cursor={:?}",
            screen.is_alt_screen(),
            screen.bracketed_paste(),
            screen.focus_reporting(),
            screen.mouse_modes(),
            screen.cursor_style(),
        ),
        cursor: screen.cursor(),
        keyboard: format!("{:?}", screen.keyboard_state()),
        progress: screen.progress(),
        image_bytes: screen.inline_image_bytes(),
    }
}

/// The stream parsed one byte at a time into the Engine's emulator
/// configuration, draining what the Engine drains as it goes.
fn reference(stream: &[u8]) -> HeadlessScreen {
    let mut screen = HeadlessScreen::new_with_keyboard_enhancements(COLS, ROWS)
        .with_notifications()
        .with_inline_images()
        .with_host_replies("vtbench");
    for (at, byte) in stream.iter().enumerate() {
        screen.feed(std::slice::from_ref(byte));
        if at % 4096 == 4095 {
            let _ = screen.take_replies();
            let _ = screen.take_notifications();
            let _ = screen.take_clipboard();
        }
    }
    screen
}

/// The program every column runs: `cat` the payload on a raw tty, then
/// print the marker and idle.
fn program(payload: &Path) -> PtySpec {
    let script = format!(
        "stty raw -echo; cat '{}'; printf '\\033[0m\\033[?1049l\\033[2J\\033[H{MARKER}'; exec sleep 600",
        payload.display()
    );
    PtySpec::new(vec!["/bin/sh".into(), "-c".into(), script], "/tmp")
        .env("PATH", "/usr/bin:/bin")
        .env("TERM", "xterm-256color")
        .size(COLS as u16, ROWS as u16)
}

/// Seconds from spawn until a bare reader has drained `expected_bytes` from
/// the same program's PTY in 64 KiB blocking reads, parsing nothing: the
/// ceiling the PTY itself puts on any terminal reading it.
fn time_pty_drain(payload: &Path, expected_bytes: u64) -> f64 {
    use std::io::Read;
    let started = Instant::now();
    let mut pty = Pty::spawn(&program(payload)).expect("spawn pty");
    let mut reader = pty.reader().expect("pty reader");
    let mut buffer = vec![0u8; 64 << 10];
    let mut drained = 0u64;
    while drained < expected_bytes {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => drained += count as u64,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => panic!("pty read: {error}"),
        }
    }
    let elapsed = started.elapsed().as_secs_f64();
    assert_eq!(drained, expected_bytes, "the PTY delivered every byte");
    drop(reader);
    let _ = pty.terminate(Duration::from_secs(2));
    elapsed
}

/// Seconds the Engine's emulator configuration takes to parse `stream` in
/// 64 KiB reads in this process, with no PTY, Holder or socket.
fn time_parse(stream: &[u8]) -> f64 {
    let mut screen = HeadlessScreen::new_with_keyboard_enhancements(COLS, ROWS)
        .with_notifications()
        .with_inline_images()
        .with_host_replies("vtbench");
    let started = Instant::now();
    for chunk in stream.chunks(64 << 10) {
        screen.feed(chunk);
        let _ = screen.take_replies();
        let _ = screen.take_notifications();
        let _ = screen.take_clipboard();
    }
    let elapsed = started.elapsed().as_secs_f64();
    std::hint::black_box(&screen);
    elapsed
}

/// Seconds from spawn until the Engine has parsed `expected_bytes`, after
/// checking the count and, given a reference, the terminal it built.
fn time_payload(
    root: &Path,
    engine: &Arc<ManifestEngine>,
    payload: &Path,
    expected_bytes: u64,
    reference: Option<&mut HeadlessScreen>,
    run: usize,
) -> f64 {
    let holder = HolderConfig {
        holders_dir: root.join("holders"),
        executable: PathBuf::from(
            std::env::var("DIRI_HOLDER_BIN").expect("set DIRI_HOLDER_BIN to a built diri-holder"),
        ),
    };
    let spec = SessionSpec {
        id: format!("s_vtbench_{run}"),
        pty: program(payload),
        manifest_id: "shell".into(),
        authority: Authority::ProcessOnly,
        logs_dir: root.join("logs"),
        holder: Some(holder),
        remote: None,
        defer_launch: false,
    };
    let started = Instant::now();
    let mut session = Session::spawn(spec, Arc::clone(engine)).expect("spawn held");
    let deadline = started + Duration::from_secs(600);
    // The counter is an atomic: polling it never takes the screen lock the
    // pump parses under. A pump that skipped bytes never reaches the count;
    // the marker on screen ends the wait so the check below can say so.
    let mut polls = 0u32;
    while session.parsed_output_bytes() < expected_bytes {
        assert!(Instant::now() < deadline, "the payload never finished");
        polls += 1;
        if polls.is_multiple_of(250) && marker_shown(&session) {
            break;
        }
        std::thread::sleep(Duration::from_micros(200));
    }
    let elapsed = started.elapsed().as_secs_f64();
    // Anything the program wrote after the marker would also be counted.
    std::thread::sleep(Duration::from_millis(50));
    let parsed = session.parsed_output_bytes();
    assert_eq!(
        parsed, expected_bytes,
        "bytes parsed by the Engine != bytes the program wrote"
    );
    assert!(
        marker_shown(&session),
        "the marker is the last thing on screen"
    );
    if let Some(reference) = reference {
        let (actual_rows, mut actual) = session.inspect_screen(|screen| {
            let rows = history_rows(screen);
            (rows, observe(screen, rows))
        });
        let reference_rows = history_rows(reference);
        let common = actual_rows.min(reference_rows);
        if common < actual_rows {
            actual = session.inspect_screen(|screen| observe(screen, common));
        }
        let expected = observe(reference, common);
        if actual != expected {
            let field = format!("{actual:?}")
                .split(", ")
                .zip(format!("{expected:?}").split(", "))
                .find(|(a, b)| a != b)
                .map(|(a, b)| format!("Engine {a:.200} vs reference {b:.200}"))
                .unwrap_or_default();
            panic!("the Engine's terminal differs from the byte-at-a-time reference: {field}");
        }
        if actual_rows != reference_rows {
            eprintln!(
                "  note: history depth {actual_rows} rows (Engine) vs {reference_rows} (reference); the newest {common} match"
            );
        }
    }
    let _ = session.terminate(Duration::from_secs(2));
    elapsed
}

fn marker_shown(session: &Session) -> bool {
    session
        .screen_lines()
        .first()
        .is_some_and(|line| line.starts_with(MARKER))
}

fn summary(mut values: Vec<f64>) -> (f64, f64, f64) {
    values.sort_by(f64::total_cmp);
    (
        values[values.len() / 2],
        values[0],
        values[values.len() - 1],
    )
}

fn main() {
    let source = PathBuf::from(
        std::env::args()
            .nth(1)
            .expect("usage: vtbench <directory of workload .bin files>"),
    );
    let runs: usize = std::env::var("DIRI_VT_BENCH_RUNS")
        .ok()
        .and_then(|runs| runs.parse().ok())
        .unwrap_or(5)
        .max(1);
    let only = std::env::var("DIRI_VT_BENCH_ONLY").ok();
    let root = std::env::temp_dir().join(format!("diri-vtbench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("work dir");
    let engine = engine();
    let mut run = 0;
    let mut next = || {
        run += 1;
        run
    };

    let suffix = suffix();
    let empty = root.join("empty.bin");
    std::fs::write(&empty, b"").expect("empty payload");
    let (baseline, _, _) = summary(
        (0..5)
            .map(|_| time_payload(&root, &engine, &empty, suffix.len() as u64, None, next()))
            .collect(),
    );
    let (pty_baseline, _, _) = summary(
        (0..5)
            .map(|_| time_pty_drain(&empty, suffix.len() as u64))
            .collect(),
    );
    println!(
        "spawn-to-last-byte baseline {:.1} ms Holder->Engine, {:.1} ms PTY drain (subtracted)",
        baseline * 1e3,
        pty_baseline * 1e3
    );
    println!(
        "MB/s, median of {runs} (min..max). Holder->Engine: every byte parsed and checked.\n\
         parse+grid: the same stream in 64 KiB reads, in process. PTY drain: a bare\n\
         reader of the same program's PTY, parsing nothing (spawn time subtracted).\n"
    );
    println!(
        "{:<18} {:>22} {:>22} {:>22} {:>8}",
        "workload", "Holder->Engine", "parse+grid", "PTY drain", "e2e/min"
    );
    for name in [
        "ascii",
        "unicode",
        "unique_unicode",
        "csi",
        "images",
        "long_escape_codes",
    ] {
        if only
            .as_deref()
            .is_some_and(|only| !only.split(',').any(|wanted| wanted == name))
        {
            continue;
        }
        let data = std::fs::read(source.join(format!("{name}.bin")))
            .unwrap_or_else(|error| panic!("{name}.bin: {error}"));
        let reps = MIN_BYTES.div_ceil(data.len());
        let mut stream = data.repeat(reps);
        let payload = root.join(format!("{name}.bin"));
        std::fs::write(&payload, &stream).expect("payload");
        let bytes = stream.len();
        stream.extend_from_slice(&suffix);
        let profiling = std::env::var_os("DIRI_VT_BENCH_PROFILE").is_some();
        let mut expected = (!profiling).then(|| reference(&stream));
        let expected_bytes = stream.len() as u64;
        let mut engine_rates = Vec::new();
        let mut parse_rates = Vec::new();
        let mut pty_rates = Vec::new();
        // Interleaved, so load that comes and goes lands on every column.
        for _ in 0..runs {
            pty_rates.push(
                bytes as f64 / (time_pty_drain(&payload, expected_bytes) - pty_baseline) / 1e6,
            );
            parse_rates.push(stream.len() as f64 / time_parse(&stream) / 1e6);
            let seconds = time_payload(
                &root,
                &engine,
                &payload,
                expected_bytes,
                expected.as_mut(),
                next(),
            ) - baseline;
            engine_rates.push(bytes as f64 / seconds / 1e6);
        }
        drop(stream);
        let column = |rates: Vec<f64>| {
            let (median, low, high) = summary(rates);
            (median, format!("{median:.1} ({low:.0}..{high:.0})"))
        };
        let (engine_median, engine_text) = column(engine_rates);
        let (parse_median, parse_text) = column(parse_rates);
        let (pty_median, pty_text) = column(pty_rates);
        if profiling {
            println!("{name:<18} profiled, not checked: no number");
        } else {
            println!(
                "{name:<18} {engine_text:>22} {parse_text:>22} {pty_text:>22} {:>7.0}%",
                100.0 * engine_median / parse_median.min(pty_median)
            );
        }
        let _ = std::fs::remove_file(&payload);
    }
    let _ = std::fs::remove_dir_all(&root);
}
