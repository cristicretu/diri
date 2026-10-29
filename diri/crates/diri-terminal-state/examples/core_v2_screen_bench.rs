//! Core v2 in the Engine's feed configuration, on all six kitten workloads.
//!
//! ```sh
//! cargo run --release -p diri-terminal-state --features core-v2,bench-verify \
//!     --example core_v2_screen_bench -- <kitten captures> <vt_workloads.golden>
//! ```
//!
//! Each run feeds `core_v2::Screen` (kitty keyboard, notifications, inline
//! images, host replies; history at most 150,000 rows and 4 MiB of live
//! bytes) the payload repeated to at least 64 MiB, in 64 KiB reads, taking
//! replies, notifications and clipboard writes after every read as the
//! Engine's pump does: the same feed path as `vt_workloads` times for the
//! current core. Every timed run must then match:
//!
//! 1. every key of the golden digest (`vt_workloads.golden`, a known-good
//!    byte-at-a-time parse): grid, all history, cursor, modes, keyboard,
//!    title, links, replies, notifications, clipboard, progress, program
//!    status, synchronized updates, every image stored (id, format, size,
//!    SHA-256) and every OSC dispatched (count, parameters, bytes). The
//!    per-byte OSC checksum is left off timed runs; one untimed run per
//!    workload turns it on and must match it too;
//! 2. a cell-for-cell comparison with the current core fed one byte at a
//!    time and keeping as many history rows (screen and all history).
//!
//! A run that fails either reports no number. Environment: `CORE_V2_ROUNDS`
//! (default 7), `CORE_V2_ONLY` (comma-separated workloads).

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::time::Instant;

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};
use diri_terminal_state::core_v2::check::compare;
use diri_terminal_state::core_v2::screen::Screen;
use diri_terminal_state::{HISTORY_ROW_CEILING, HeadlessScreen, osc_observe};
use sha2::{Digest, Sha256};

const LINES: usize = 80;
const COLS: usize = 120;
const READ: usize = 64 << 10;
const MIN_BYTES: usize = 64 << 20;
const BUDGET: usize = 4 << 20;
const WORKLOADS: [&str; 6] = [
    "ascii",
    "unicode",
    "unique_unicode",
    "csi",
    "images",
    "long_escape_codes",
];

fn sha256(bytes: &[u8]) -> String {
    let mut hex = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

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

fn screen() -> Screen {
    Screen::new(
        LINES,
        COLS,
        HISTORY_ROW_CEILING,
        Some(BUDGET),
        true,
        Some("bench".to_owned()),
    )
    .with_notifications()
    .with_inline_images()
}

struct Run {
    screen: Screen,
    bytes_fed: usize,
    replies: Vec<u8>,
    notifications: Vec<String>,
    clipboard: Vec<String>,
}

/// One run, as `vt_workloads::run` feeds the current core.
fn run(data: &[u8], reps: usize, checksum: bool) -> (f64, Option<(u64, u64)>, Run) {
    let mut out = Run {
        screen: screen(),
        bytes_fed: 0,
        replies: Vec::new(),
        notifications: Vec::new(),
        clipboard: Vec::new(),
    };
    osc_observe::reset(checksum);
    let counters = cpu_counters();
    let started = Instant::now();
    for _ in 0..reps {
        for chunk in data.chunks(READ) {
            out.screen.feed(std::hint::black_box(chunk));
            out.bytes_fed += chunk.len();
            out.replies.extend(out.screen.take_replies());
            let notifications = out.screen.take_notifications();
            if !notifications.is_empty() {
                out.notifications
                    .extend(notifications.iter().map(|n| format!("{n:?}")));
            }
            out.clipboard.extend(out.screen.take_clipboard());
        }
    }
    let secs = started.elapsed().as_secs_f64();
    let counted = counters
        .zip(cpu_counters())
        .map(|((c0, i0), (c1, i1))| (c1 - c0, i1 - i0));
    (secs, counted, out)
}

/// `vt_workloads::without_history_position`: the snapshot's history
/// position (how many rows are kept) is compared through the history keys.
fn without_history_position(mut text: String) -> String {
    if let Some(at) = text.rfind(", history: ") {
        text.replace_range(at..text.len() - " }".len(), "");
    }
    text
}

/// Image anchors name a per-screen store nonce; only the rest must match.
fn without_store_nonce(text: String) -> String {
    const ANCHOR: &str = "diri-image:";
    let mut out = String::with_capacity(text.len());
    let mut rest = text.as_str();
    while let Some(at) = rest.find(ANCHOR) {
        out.push_str(&rest[..at + ANCHOR.len()]);
        rest = &rest[at + ANCHOR.len()..];
        let nonce = rest.find('.').unwrap_or(0);
        out.push('#');
        rest = &rest[nonce..];
    }
    out.push_str(rest);
    out
}

/// `vt_workloads::digest`, key for key, over the v2 screen (grid, history
/// and links through `HeadlessScreen::from_core_v2`).
fn digest(run: &mut Run, checked: usize) -> BTreeMap<String, String> {
    let mut d = BTreeMap::new();
    let mut put = |key: &str, value: String| {
        d.insert(key.to_owned(), value);
    };
    put("bytes_fed", run.bytes_fed.to_string());
    let osc = osc_observe::totals();
    put(
        "osc_dispatches",
        format!(
            "{} params={} param_bytes={} bel={}",
            osc.dispatches, osc.params, osc.param_bytes, osc.bell_terminated
        ),
    );
    put("osc_checksum", format!("{:016x}", osc.checksum));
    let log = run.screen.take_stored_image_log();
    let mut images = format!("count={}", log.len());
    let mut previous: Option<(String, usize)> = None;
    let flush = |images: &mut String, entry: Option<(String, usize)>| {
        if let Some((text, times)) = entry {
            let _ = write!(images, " [{text}]x{times}");
        }
    };
    for (kitty_id, image) in &log {
        let text = format!(
            "id={} {:?}{} {}x{} {}B sha256={}",
            kitty_id.map_or_else(|| "-".to_owned(), |id| id.to_string()),
            image.format,
            if image.compressed { " z" } else { "" },
            image.width,
            image.height,
            image.data.len(),
            sha256(&image.data)
        );
        match &mut previous {
            Some((last, times)) if *last == text => *times += 1,
            _ => flush(&mut images, previous.replace((text, 1))),
        }
    }
    flush(&mut images, previous);
    put("images_stored", images);
    put(
        "images_held_bytes",
        run.screen.inline_image_bytes().to_string(),
    );
    put(
        "replies",
        format!("{}B sha256={}", run.replies.len(), sha256(&run.replies)),
    );
    put(
        "notifications",
        format!(
            "{} sha256={}",
            run.notifications.len(),
            sha256(run.notifications.join("\n").as_bytes())
        ),
    );
    put(
        "clipboard",
        format!(
            "{} sha256={}",
            run.clipboard.len(),
            sha256(run.clipboard.join("\n").as_bytes())
        ),
    );

    let screen = &run.screen;
    let mut bridged = HeadlessScreen::from_core_v2(&screen.core);
    put(
        "grid_sha256",
        sha256(
            without_history_position(without_store_nonce(format!(
                "{:?}",
                bridged.full_snapshot()
            )))
            .as_bytes(),
        ),
    );
    let kept = bridged.history_rows();
    put(
        "history_rows_checked",
        if kept >= checked {
            checked.to_string()
        } else {
            format!("{checked} but only {kept} kept")
        },
    );
    let scrollback = bridged.scrollback();
    let skip = scrollback.lines.len().saturating_sub(checked + LINES);
    let mut text = scrollback.lines[skip..].join("\n");
    let text_cells: Vec<_> = scrollback
        .text_cells
        .range(skip..)
        .map(|(row, cells)| (row - skip, cells))
        .collect();
    let _ = write!(text, "\n{text_cells:?}\n{}", screen.is_alt_screen());
    put("scrollback_text_sha256", sha256(text.as_bytes()));
    let cells = bridged.scrollback_cells(
        (kept.saturating_sub(checked)) as i64,
        (checked + LINES) as i64,
    );
    put("scrollback_cells_sha256", sha256(&cells.payload));
    put(
        "scrollback_metadata_sha256",
        sha256(without_store_nonce(format!("{:?}", cells.metadata)).as_bytes()),
    );
    let links = bridged.link_source(checked);
    let link_list: Vec<String> = links
        .hyperlinks
        .into_iter()
        .map(|(at, uri)| format!("{at} {}", without_store_nonce(uri)))
        .collect();
    put(
        "links",
        format!(
            "{} sha256={}",
            link_list.len(),
            sha256(format!("{}\n{}", links.text, link_list.join("\n")).as_bytes())
        ),
    );
    put("title", format!("{:?}", screen.title()));
    put("cursor", format!("{:?}", screen.cursor()));
    put(
        "modes",
        format!(
            "alt={} paste={} focus={} mouse={:?} cursor={:?}",
            screen.is_alt_screen(),
            screen.bracketed_paste(),
            screen.focus_reporting(),
            screen.mouse_modes(),
            screen.cursor_style(),
        ),
    );
    put("keyboard", format!("{:?}", screen.keyboard_state()));
    put("progress", format!("{:?}", screen.progress()));
    put("program_status", format!("{:?}", screen.program_status()));
    put(
        "synchronized",
        format!(
            "completed_any={} open={}",
            screen.synchronized_updates_completed() > 0,
            screen.in_synchronized_update()
        ),
    );
    d
}

/// The keys of `got` that differ from the golden section (the OSC
/// checksum only when `checksum`).
fn mismatches(
    got: &BTreeMap<String, String>,
    want: &BTreeMap<String, String>,
    checksum: bool,
) -> Vec<String> {
    got.iter()
        .filter(|(key, _)| checksum || key.as_str() != "osc_checksum")
        .filter(|(key, value)| want.get(key.as_str()) != Some(value))
        .map(|(key, value)| {
            format!(
                "{key}: got {value}, golden {}",
                want.get(key.as_str()).map_or("(missing)", String::as_str)
            )
        })
        .collect()
}

/// This process's (cycles, instructions) so far, from macOS's per-process
/// counters (which count only while it runs). `None` elsewhere.
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
        "{:.0} ({:.0}..{:.0})",
        values[values.len() / 2],
        values[0],
        values[values.len() - 1]
    )
}

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args
        .next()
        .expect("usage: core_v2_screen_bench <dir> <golden>");
    let golden = read_golden(&args.next().expect("golden file"));
    let rounds: usize = std::env::var("CORE_V2_ROUNDS")
        .ok()
        .map_or(7, |v| v.parse().expect("rounds"));
    let only = std::env::var("CORE_V2_ONLY").ok();
    println!(
        "{LINES}x{COLS}, history <= {HISTORY_ROW_CEILING} rows and {BUDGET} bytes, {READ}-byte reads, >= {} MiB per run, {rounds} rounds",
        MIN_BYTES >> 20
    );
    for name in WORKLOADS {
        if only
            .as_deref()
            .is_some_and(|only| !only.split(',').any(|o| o == name))
        {
            continue;
        }
        let payload = std::fs::read(format!("{dir}/{name}.bin")).expect("payload");
        let reps = MIN_BYTES.div_ceil(payload.len());
        let key = format!("{name} {}", sha256(&payload));
        let want = golden
            .get(&key)
            .unwrap_or_else(|| panic!("no golden section [{key}]"));
        assert_eq!(want["reps"], reps.to_string(), "golden repetitions");
        let checked: usize = want["history_rows_checked"].parse().expect("rows");

        // Untimed: the per-byte OSC checksum, and the rows v2 keeps for
        // the byte-at-a-time reference.
        let (_, _, mut first) = run(&payload, reps, true);
        let kept = first.screen.core.history_size();
        let mut failure = mismatches(&digest(&mut first, checked), want, true);
        drop(first);
        let mut reference = {
            let config = Config {
                scrolling_history: kept,
                ..Config::default()
            };
            let mut term = Term::new(config, &TermSize::new(COLS, LINES), VoidListener);
            term.grid_mut().enable_compact_history();
            term
        };
        let mut parser = Processor::<StdSyncHandler>::new();
        for _ in 0..reps {
            for byte in payload.chunks(1) {
                parser.advance(&mut reference, byte);
            }
        }

        let mut rates = Vec::new();
        let (mut cycles, mut instructions) = (Vec::new(), Vec::new());
        for _ in 0..rounds {
            if !failure.is_empty() {
                break;
            }
            let (secs, counted, mut out) = run(&payload, reps, false);
            let bytes = payload.len() * reps;
            if out.bytes_fed != bytes {
                failure.push(format!("fed {} of {bytes} bytes", out.bytes_fed));
                break;
            }
            if out.screen.unanswered_events != 0 {
                failure.push(format!(
                    "{} unanswered events",
                    out.screen.unanswered_events
                ));
            }
            failure.extend(mismatches(&digest(&mut out, checked), want, false));
            if let Err(error) = compare(&out.screen.core, &reference) {
                failure.push(format!("cell comparison: {error}"));
            }
            out.screen.core.release_read_cache();
            if failure.is_empty() {
                rates.push(bytes as f64 / secs / 1e6);
                if let Some((c, i)) = counted {
                    cycles.push(c as f64 / bytes as f64);
                    instructions.push(i as f64 / bytes as f64);
                }
            }
        }
        if failure.is_empty() {
            let per_byte = |mut v: Vec<f64>| {
                if v.is_empty() {
                    return "-".to_owned();
                }
                v.sort_by(f64::total_cmp);
                format!("{:.3} ({:.3}..{:.3})", v[v.len() / 2], v[0], v[v.len() - 1])
            };
            println!(
                "{name:<18} {:>22} MB/s  cycles/B {}  instructions/B {}  rows {kept}  golden ok x{} + checksum",
                summary(rates),
                per_byte(cycles),
                per_byte(instructions),
                rounds
            );
        } else {
            println!("{name:<18} FAILED: {}", failure.join("; "));
        }
    }
}
