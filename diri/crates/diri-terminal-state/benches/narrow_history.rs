//! Per-line output cost after a terminal holding deep history narrows.
//!
//! Fills 80-column history to each depth, times lines fed one at a time (as
//! PTY reads deliver them) before the resize, narrows 80 → 40, then times
//! lines again. Each depth runs in its own child process so RSS is its own.
//!
//! `cargo bench -p diri-terminal-state --bench narrow_history [-- <rows>...]`
//! measures release builds; add `--profile dev` for the debug build that
//! `cargo test` and development app builds run. `NARROW_KIND=prose` feeds
//! word-wrapped prose (default: numbered 70-column lines) and
//! `NARROW_GATE=<factor>` fails when post-narrow lines cost more than
//! `factor` times the pre-narrow lines at any depth.
use std::hint::black_box;
use std::process::Command;
use std::time::{Duration, Instant};

use diri_terminal_state::HeadlessScreen;

const SCREEN_ROWS: usize = 50;
const TIMED_LINES: usize = 2_000;
const BATCHES: usize = 5;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

const WORDS: &[&str] = &[
    "the", "session", "holder", "terminal", "returns", "history", "budget", "rows", "parser",
    "error", "test", "passes", "update", "config", "value", "file", "remote", "cursor", "ok",
];

fn line(prose: bool, rng: &mut Rng, i: usize) -> Vec<u8> {
    if !prose {
        let text = format!("L{i:06} {}", "abcdefghij".repeat(7));
        return format!("{}\r\n", &text[..70]).into_bytes();
    }
    let mut out = format!("{i:>7} ");
    while out.len() < 50 + (rng.next() % 28) as usize {
        out.push_str(WORDS[(rng.next() % WORDS.len() as u64) as usize]);
        out.push(' ');
    }
    out.push_str("\r\n");
    out.into_bytes()
}

/// Median over batches of the mean cost of one line fed on its own.
fn per_line(screen: &mut HeadlessScreen, prose: bool, rng: &mut Rng, next: &mut usize) -> Duration {
    let mut batches = Vec::with_capacity(BATCHES);
    for _ in 0..BATCHES {
        let lines: Vec<Vec<u8>> = (0..TIMED_LINES)
            .map(|offset| line(prose, rng, *next + offset))
            .collect();
        *next += TIMED_LINES;
        let begin = Instant::now();
        for line in &lines {
            screen.feed(line);
        }
        batches.push(begin.elapsed() / TIMED_LINES as u32);
    }
    batches.sort();
    batches[BATCHES / 2]
}

fn rss_kib() -> u64 {
    let out = Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .expect("ps");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or(0)
}

fn run(rows: usize) {
    let prose = std::env::var("NARROW_KIND").is_ok_and(|kind| kind == "prose");
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    // Configured as the Engine configures a session's screen.
    let mut screen = HeadlessScreen::new(80, SCREEN_ROWS)
        .with_notifications()
        .with_host_replies("bench")
        .keeping_spare_rows_until_idle();
    let mut fill = Vec::new();
    for i in 0..rows + SCREEN_ROWS {
        fill.extend(line(prose, &mut rng, i));
    }
    for chunk in fill.chunks(64 * 1024) {
        screen.feed(chunk);
    }
    drop(fill);
    let held = screen.history_rows();
    let mut next = rows + SCREEN_ROWS;
    let before = per_line(&mut screen, prose, &mut rng, &mut next);
    let begin = Instant::now();
    screen.resize(40, SCREEN_ROWS);
    black_box(screen.grid_update(false));
    let resize = begin.elapsed();
    let narrowed = screen.history_rows();
    let after = per_line(&mut screen, prose, &mut rng, &mut next);
    println!(
        "history {held:>7} -> {narrowed:>7} rows  before {:>7.2} us/line  narrow {:>7.1} ms  after {:>7.2} us/line  stored {:>5} KiB  rss {:>6} KiB",
        before.as_secs_f64() * 1e6,
        resize.as_secs_f64() * 1e3,
        after.as_secs_f64() * 1e6,
        screen.history_storage_bytes() >> 10,
        rss_kib(),
    );
    if let Some(factor) = std::env::var("NARROW_GATE")
        .ok()
        .and_then(|factor| factor.parse::<f64>().ok())
    {
        assert!(
            after.as_secs_f64() <= before.as_secs_f64() * factor,
            "a line after narrowing {held} rows costs {after:?}, {before:?} before"
        );
    }
}

fn main() {
    if let Ok(rows) = std::env::var("NARROW_HISTORY_CHILD") {
        run(rows.parse().expect("rows"));
        return;
    }
    let depths: Vec<usize> = std::env::args()
        .skip(1)
        .filter_map(|arg| arg.parse().ok())
        .collect();
    let depths = if depths.is_empty() {
        vec![10_000, 50_000, 150_000]
    } else {
        depths
    };
    for rows in depths {
        let status = Command::new(std::env::current_exe().expect("exe"))
            .env("NARROW_HISTORY_CHILD", rows.to_string())
            .status()
            .expect("child");
        assert!(status.success());
    }
}
