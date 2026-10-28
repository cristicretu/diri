//! Core v2 prototype against the current core, on captured payloads.
//!
//! ```sh
//! cargo run --release -p diri-terminal-state --features core-v2 \
//!     --example core_v2_bench -- <dir with ascii.bin unicode.bin csi.bin>
//! ```
//!
//! Environment: `CORE_V2_HISTORY` (history rows kept by both cores,
//! default 10000; 0 turns history off), `CORE_V2_ROUNDS` (default 7),
//! `CORE_V2_ONLY` (one workload name), `CORE_V2_NO_RAW` (v2 takes text from the
//! parser's validated runs instead of raw bytes), `CORE_V2_ONLY_V2` (time only v2,
//! for profiling), `CORE_V2_GOLDEN` (path to `vt_workloads.golden`: every
//! timed v2 run must also match its grid, history, cursor and link digests;
//! needs a history of at least the golden's checked rows).
//!
//! Per workload the payload is repeated to at least 64 MiB and fed in
//! 64 KiB reads. Rounds interleave the three cores (v2, current dense,
//! current compact), and every timed v2 run must end in exactly the state
//! a byte-at-a-time parse by the current core reaches: every cell's
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
    let mut d = BTreeMap::new();
    let mut screen = HeadlessScreen::from_core_v2(core);
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

fn summary(mut values: Vec<f64>) -> String {
    values.sort_by(f64::total_cmp);
    format!(
        "{:7.0} ({:.0}..{:.0})",
        values[values.len() / 2],
        values[0],
        values[values.len() - 1]
    )
}

fn main() {
    let dir = std::env::args().nth(1).expect("usage: core_v2_bench <dir>");
    let history: usize = std::env::var("CORE_V2_HISTORY")
        .ok()
        .map_or(10_000, |v| v.parse().expect("rows"));
    let rounds: usize = std::env::var("CORE_V2_ROUNDS")
        .ok()
        .map_or(7, |v| v.parse().expect("rounds"));
    let only = std::env::var("CORE_V2_ONLY").ok();
    let golden = std::env::var("CORE_V2_GOLDEN")
        .ok()
        .map(|path| read_golden(&path));
    println!(
        "{LINES}x{COLS}, history {history} rows, {READ}-byte reads, >= {} MiB per run, {rounds} rounds",
        MIN_BYTES >> 20
    );
    println!(
        "{:<10} {:>22} {:>22} {:>22}",
        "workload", "v2 MB/s", "current dense", "current compact"
    );
    for name in ["ascii", "unicode", "csi"] {
        if only.as_deref().is_some_and(|o| o != name) {
            continue;
        }
        let payload = std::fs::read(format!("{dir}/{name}.bin")).expect("payload");
        let reps = MIN_BYTES.div_ceil(payload.len());
        let bytes = payload.len() * reps;

        // Reference: the current core, one byte per read.
        let mut reference = term(history, false);
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

        let (mut v2, mut dense, mut compact) = (Vec::new(), Vec::new(), Vec::new());
        let mut failure = None;
        for _ in 0..rounds {
            let mut core = Core::new(LINES, COLS, history);
            core.raw_text = std::env::var_os("CORE_V2_NO_RAW").is_none();
            let start = Instant::now();
            let fed = feed(&mut core, &payload, reps);
            let secs = start.elapsed().as_secs_f64();
            assert_eq!(fed, bytes, "every byte is fed");
            if let Some(want) = &want {
                let checked: usize = want["history_rows_checked"].parse().expect("rows");
                for (key, value) in golden_digest(&core, fed, checked) {
                    if want[&key] != value {
                        failure = Some(format!(
                            "golden {key}: got {value}, reference {}",
                            want[&key]
                        ));
                    }
                }
                if failure.is_some() {
                    break;
                }
                golden_checked += 1;
            }
            match compare(&core, &reference) {
                Ok(()) => v2.push(bytes as f64 / secs / 1e6),
                Err(error) => {
                    failure = Some(error);
                    break;
                }
            }
            drop(core);

            if std::env::var_os("CORE_V2_ONLY_V2").is_some() {
                dense.push(0.0);
                compact.push(0.0);
                continue;
            }
            for (out, is_compact) in [(&mut dense, false), (&mut compact, true)] {
                let mut current = term(history, is_compact);
                let start = Instant::now();
                let fed = feed(&mut current, &payload, reps);
                let secs = start.elapsed().as_secs_f64();
                assert_eq!(fed, bytes, "every byte is fed");
                out.push(bytes as f64 / secs / 1e6);
            }
        }
        match failure {
            Some(error) => println!("{name:<10} v2 FAILED THE STATE CHECK: {error}"),
            None => println!(
                "{name:<10} {:>22} {:>22} {:>22}{}",
                summary(v2),
                summary(dense),
                summary(compact),
                if golden_checked > 0 {
                    format!("  golden ok x{golden_checked}")
                } else {
                    String::new()
                }
            ),
        }
    }
}
