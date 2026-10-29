//! Core v2 prototype against the current core, on captured payloads.
//!
//! ```sh
//! cargo run --release -p diri-terminal-state --features core-v2 \
//!     --example core_v2_bench -- <dir with ascii.bin unicode.bin csi.bin>
//! ```
//!
//! Environment: `CORE_V2_HISTORY` (history row limit, default the Engine's
//! 150,000; 0 turns history off), `CORE_V2_BUDGET` (live history bytes,
//! default the Engine's 4 MiB; `none` keeps history dense), `CORE_V2_RSS`
//! (`v2` or `engine`: peak RSS of that core alone over `CORE_V2_ONLY`),
//! `CORE_V2_ENGINE_GOLDEN` (with `CORE_V2_GOLDEN`: check the Engine's own
//! screen against the golden digests on all six workloads), `CORE_V2_ROUNDS` (default 7),
//! `CORE_V2_ONLY` (one workload name), `CORE_V2_NO_RAW` (v2 takes text from the
//! parser's validated runs instead of raw bytes), `CORE_V2_ONLY_V2` (time only v2,
//! for profiling), `CORE_V2_GOLDEN` (path to `vt_workloads.golden`: every
//! timed v2 run must also match its grid, history, cursor and link digests;
//! needs a history of at least the golden's checked rows).
//!
//! Per workload the payload is repeated to at least 64 MiB and fed in
//! 64 KiB reads. Rounds interleave v2 and the Engine's `HeadlessScreen`,
//! and every timed v2 run must end in exactly the state a byte-at-a-time
//! parse by the current core (keeping as many history rows) reaches: every cell's
//! character, cluster, width flags, wrap flag, style flags and colors, over
//! the whole screen and the whole retained history, plus the cursor. A run
//! that does not match, or that fed fewer bytes than the payload holds,
//! reports no number.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::time::Instant;

use diri_terminal_state::HeadlessScreen;
use sha2::{Digest, Sha256};

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};
use diri_terminal_state::core_v2::Core;
use diri_terminal_state::core_v2::check::compare;

const LINES: usize = 80;
const COLS: usize = 120;
const READ: usize = 64 << 10;
const MIN_BYTES: usize = 64 << 20;

fn term(history: usize, compact: bool) -> Term<VoidListener> {
    let config = Config {
        scrolling_history: history,
        ..Config::default()
    };
    let mut term = Term::new(config, &TermSize::new(COLS, LINES), VoidListener);
    if compact {
        term.grid_mut().enable_compact_history();
    }
    term
}

/// Feeds `payload` `reps` times in `READ`-byte reads; returns bytes fed.
fn feed<H: alacritty_terminal::vte::ansi::Handler>(
    handler: &mut H,
    payload: &[u8],
    reps: usize,
) -> usize {
    let mut parser = Processor::<StdSyncHandler>::new();
    let mut fed = 0;
    for _ in 0..reps {
        for chunk in payload.chunks(READ) {
            parser.advance(handler, chunk);
            fed += chunk.len();
        }
    }
    fed
}

fn sha256(bytes: &[u8]) -> String {
    let mut hex = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// Golden sections keyed by `workload payload_sha256`, from the reference
/// file of `benches/vt_workloads.golden` (a known-good byte-at-a-time parse).
fn read_golden(path: &str) -> BTreeMap<String, BTreeMap<String, String>> {
    let text = std::fs::read_to_string(path).expect("golden file");
    let mut golden = BTreeMap::<String, BTreeMap<String, String>>::new();
    let mut section = None;
    for line in text.lines() {
        if let Some(key) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = Some(key.to_owned());
        } else if let (Some(key), Some((field, value))) = (&section, line.split_once(" = ")) {
            golden
                .entry(key.clone())
                .or_default()
                .insert(field.to_owned(), value.to_owned());
        }
    }
    golden
}

/// The golden digest's grid, history, cursor and link keys, computed the
/// way `vt_workloads` computes them, over the v2 state seen through
/// `HeadlessScreen`. History is compared over the newest `checked` rows.
fn golden_digest(core: &Core, bytes_fed: usize, checked: usize) -> BTreeMap<String, String> {
    screen_digest(&mut HeadlessScreen::from_core_v2(core), bytes_fed, checked)
}

/// The golden keys of any screen: the v2 core through the bridge, or the
/// Engine's own screen.
fn screen_digest(
    screen: &mut HeadlessScreen,
    bytes_fed: usize,
    checked: usize,
) -> BTreeMap<String, String> {
    let mut d = BTreeMap::new();
    d.insert("bytes_fed".into(), bytes_fed.to_string());
    d.insert(
        "grid_sha256".into(),
        sha256(format!("{:?}", screen.full_snapshot()).as_bytes()),
    );
    let kept = screen.history_rows();
    d.insert(
        "history_rows_checked".into(),
        if kept >= checked {
            checked.to_string()
        } else {
            format!("{checked} but only {kept} kept")
        },
    );
    let scrollback = screen.scrollback();
    let skip = scrollback.lines.len().saturating_sub(checked + LINES);
    let mut text = scrollback.lines[skip..].join("\n");
    let text_cells: Vec<_> = scrollback
        .text_cells
        .range(skip..)
        .map(|(row, cells)| (row - skip, cells))
        .collect();
    let _ = write!(text, "\n{text_cells:?}\n{}", scrollback.is_alt_screen);
    d.insert("scrollback_text_sha256".into(), sha256(text.as_bytes()));
    let cells = screen.scrollback_cells(
        kept.saturating_sub(checked) as i64,
        (checked + LINES) as i64,
    );
    d.insert("scrollback_cells_sha256".into(), sha256(&cells.payload));
    d.insert(
        "scrollback_metadata_sha256".into(),
        sha256(format!("{:?}", cells.metadata).as_bytes()),
    );
    let links = screen.link_source(checked);
    let link_list: Vec<String> = links
        .hyperlinks
        .into_iter()
        .map(|(at, uri)| format!("{at} {uri}"))
        .collect();
    d.insert(
        "links".into(),
        format!(
            "{} sha256={}",
            link_list.len(),
            sha256(format!("{}\n{}", links.text, link_list.join("\n")).as_bytes())
        ),
    );
    d.insert("cursor".into(), format!("{:?}", screen.cursor()));
    d
}

/// This process's (cycles, instructions) so far: macOS's per-process
/// counters, which count only while it runs, so a loaded machine changes
/// them far less than wall time. `None` elsewhere.
fn cpu_counters() -> Option<(u64, u64)> {
    #[cfg(target_os = "macos")]
    {
        let mut info: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
        // SAFETY: the buffer is a `rusage_info_v4`, the flavor asked for.
        let ok = unsafe {
            libc::proc_pid_rusage(
                std::process::id() as i32,
                libc::RUSAGE_INFO_V4,
                &mut info as *mut _ as *mut libc::rusage_info_t,
            )
        } == 0;
        ok.then_some((info.ri_cycles, info.ri_instructions))
    }
    #[cfg(not(target_os = "macos"))]
    None
}

fn summary(mut values: Vec<f64>) -> String {
    values.sort_by(f64::total_cmp);
    format!(
        "{:7.0} ({:.0}..{:.0})",
        values[values.len() / 2],
        values[0],
        values[values.len() - 1]
    )
}

/// The Engine's terminal: `HeadlessScreen` (compact history bounded at
/// 4 MiB, notifications, links, images, replies), fed per read as the
/// Holder feeds it.
fn feed_engine(screen: &mut HeadlessScreen, payload: &[u8], reps: usize) -> usize {
    let mut fed = 0;
    for _ in 0..reps {
        for chunk in payload.chunks(READ) {
            screen.feed(chunk);
            fed += chunk.len();
            let _ = screen.take_replies();
            let _ = screen.take_notifications();
            let _ = screen.take_clipboard();
        }
    }
    fed
}

fn max_rss_bytes() -> u64 {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: getrusage fills the struct it is given.
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    // Bytes on macOS, KiB on Linux.
    let rss = usage.ru_maxrss as u64;
    if cfg!(target_os = "macos") {
        rss
    } else {
        rss * 1024
    }
}

/// Settings: `CORE_V2_HISTORY` rows (default 150,000, the Engine's row
/// ceiling), `CORE_V2_BUDGET` live history bytes (default 4 MiB, the
/// Engine's budget; `none` keeps every row dense).
fn settings() -> (usize, Option<usize>) {
    let history = std::env::var("CORE_V2_HISTORY")
        .ok()
        .map_or(diri_terminal_state::HISTORY_ROW_CEILING, |v| {
            v.parse().expect("rows")
        });
    let budget = match std::env::var("CORE_V2_BUDGET").ok().as_deref() {
        None => Some(4 << 20),
        Some("none") => None,
        Some(bytes) => Some(bytes.parse().expect("bytes")),
    };
    (history, budget)
}

fn main() {
    let dir = std::env::args().nth(1).expect("usage: core_v2_bench <dir>");
    let (history, budget) = settings();
    let rounds: usize = std::env::var("CORE_V2_ROUNDS")
        .ok()
        .map_or(7, |v| v.parse().expect("rounds"));
    let only = std::env::var("CORE_V2_ONLY").ok();

    // Peak RSS of one core alone over one workload, in a fresh process.
    if let Ok(which) = std::env::var("CORE_V2_RSS") {
        let name = only.expect("CORE_V2_ONLY names the workload");
        let payload = std::fs::read(format!("{dir}/{name}.bin")).expect("payload");
        let reps = MIN_BYTES.div_ceil(payload.len());
        let before = max_rss_bytes();
        let (fed, history_rows, stored) = match which.as_str() {
            "v2" => {
                let mut core = Core::with_budget(LINES, COLS, history, budget);
                let fed = feed(&mut core, &payload, reps);
                (fed, core.history_size(), core.history_bytes())
            }
            "engine" => {
                let mut screen = HeadlessScreen::new(COLS, LINES);
                let fed = feed_engine(&mut screen, &payload, reps);
                (fed, screen.history_rows(), 0)
            }
            other => panic!("CORE_V2_RSS is v2 or engine, not {other}"),
        };
        println!(
            "{which} {name}: fed {fed} B, peak RSS {:.1} MB (process start {:.1} MB), history {history_rows} rows, v2 live history {:.2} MiB",
            max_rss_bytes() as f64 / 1e6,
            before as f64 / 1e6,
            stored as f64 / (1 << 20) as f64,
        );
        return;
    }

    let golden = std::env::var("CORE_V2_GOLDEN")
        .ok()
        .map(|path| read_golden(&path));

    // The Engine's own screen (switch off) against the golden digests, on
    // all six workloads: the parser hooks must leave its output identical.
    if std::env::var_os("CORE_V2_ENGINE_GOLDEN").is_some() {
        let golden = golden.expect("CORE_V2_GOLDEN");
        for name in [
            "ascii",
            "unicode",
            "unique_unicode",
            "csi",
            "images",
            "long_escape_codes",
        ] {
            let payload = std::fs::read(format!("{dir}/{name}.bin")).expect("payload");
            let reps = MIN_BYTES.div_ceil(payload.len());
            let key = format!("{name} {}", sha256(&payload));
            let want = golden
                .get(&key)
                .unwrap_or_else(|| panic!("no golden section [{key}]"));
            let checked: usize = want["history_rows_checked"].parse().expect("rows");
            let mut screen = HeadlessScreen::new(COLS, LINES);
            let fed = feed_engine(&mut screen, &payload, reps);
            let mut bad = Vec::new();
            for (k, value) in screen_digest(&mut screen, fed, checked) {
                if want[&k] != value {
                    bad.push(k);
                }
            }
            println!(
                "engine {name:<18} {}",
                if bad.is_empty() {
                    "golden ok".to_owned()
                } else {
                    format!("GOLDEN MISMATCH {bad:?}")
                }
            );
        }
        return;
    }
    println!(
        "{LINES}x{COLS}, history <= {history} rows, budget {}, {READ}-byte reads, >= {} MiB per run, {rounds} rounds",
        budget.map_or("none".to_owned(), |b| format!("{} bytes", b)),
        MIN_BYTES >> 20
    );
    println!(
        "{:<10} {:>20} {:>20} {:>12} {:>14}",
        "workload", "v2 MB/s", "Engine MB/s", "v2 rows", "v2 history MiB"
    );
    for name in ["ascii", "unicode", "csi"] {
        if only.as_deref().is_some_and(|o| o != name) {
            continue;
        }
        let payload = std::fs::read(format!("{dir}/{name}.bin")).expect("payload");
        let reps = MIN_BYTES.div_ceil(payload.len());
        let bytes = payload.len() * reps;

        // How many rows v2 keeps (deterministic), then the reference: the
        // current core keeping exactly that many, one byte per read.
        let kept = {
            let mut core = Core::with_budget(LINES, COLS, history, budget);
            feed(&mut core, &payload, reps);
            core.history_size()
        };
        // Compact storage keeps the reference small; reads decode it.
        let mut reference = term(kept, true);
        let mut parser = Processor::<StdSyncHandler>::new();
        for _ in 0..reps {
            for byte in payload.chunks(1) {
                parser.advance(&mut reference, byte);
            }
        }

        // The golden section for this payload, when a golden file is given.
        let want = golden.as_ref().map(|golden| {
            let key = format!("{name} {}", sha256(&payload));
            let want = golden
                .get(&key)
                .unwrap_or_else(|| panic!("no golden section [{key}]"))
                .clone();
            assert_eq!(
                want["reps"],
                reps.to_string(),
                "same repetitions as the golden run"
            );
            want
        });
        let mut golden_checked = 0;

        let (mut v2, mut engine) = (Vec::new(), Vec::new());
        // Cycles and instructions per byte of the runs that passed.
        let (mut cycles, mut instructions) = (Vec::new(), Vec::new());
        let mut failure = None;
        let mut stored = 0;
        for _ in 0..rounds {
            let mut core = Core::with_budget(LINES, COLS, history, budget);
            core.raw_text = std::env::var_os("CORE_V2_NO_RAW").is_none();
            let counters = cpu_counters();
            let start = Instant::now();
            let fed = feed(&mut core, &payload, reps);
            let secs = start.elapsed().as_secs_f64();
            let counted = counters.zip(cpu_counters()).map(|((c0, i0), (c1, i1))| {
                (
                    (c1 - c0) as f64 / bytes as f64,
                    (i1 - i0) as f64 / bytes as f64,
                )
            });
            assert_eq!(fed, bytes, "every byte is fed");
            stored = core.history_bytes();
            if let Some(want) = &want {
                let checked: usize = want["history_rows_checked"].parse().expect("rows");
                let bad: Vec<String> = golden_digest(&core, fed, checked)
                    .into_iter()
                    .filter(|(key, value)| want[key] != *value)
                    .map(|(key, value)| format!("{key}: got {value}, reference {}", want[&key]))
                    .collect();
                if !bad.is_empty() {
                    failure = Some(format!("golden {}", bad.join("; ")));
                }
                if failure.is_some() {
                    break;
                }
                golden_checked += 1;
            }
            match compare(&core, &reference) {
                Ok(()) => {
                    v2.push(bytes as f64 / secs / 1e6);
                    if let Some((c, i)) = counted {
                        cycles.push(c);
                        instructions.push(i);
                    }
                }
                Err(error) => {
                    failure = Some(error);
                    break;
                }
            }
            core.release_read_cache();
            drop(core);

            if std::env::var_os("CORE_V2_ONLY_V2").is_some() {
                engine.push(0.0);
                continue;
            }
            let mut screen = HeadlessScreen::new(COLS, LINES);
            let start = Instant::now();
            let fed = feed_engine(&mut screen, &payload, reps);
            let secs = start.elapsed().as_secs_f64();
            assert_eq!(fed, bytes, "every byte is fed");
            engine.push(bytes as f64 / secs / 1e6);
        }
        match failure {
            Some(error) => println!("{name:<10} v2 FAILED THE STATE CHECK: {error}"),
            None => println!(
                "{name:<10} {:>20} {:>20} {kept:>12} {:>14.2}{}{}",
                summary(v2),
                summary(engine),
                stored as f64 / (1 << 20) as f64,
                if golden_checked > 0 {
                    format!("  golden ok x{golden_checked}")
                } else {
                    String::new()
                },
                if cycles.is_empty() {
                    String::new()
                } else {
                    let fmt = |mut v: Vec<f64>| {
                        v.sort_by(f64::total_cmp);
                        format!("{:.3} ({:.3}..{:.3})", v[v.len() / 2], v[0], v[v.len() - 1])
                    };
                    format!(
                        "  cycles/B {}  instructions/B {}",
                        fmt(cycles),
                        fmt(instructions)
                    )
                }
            ),
        }
    }
}
