//! Cost of `search.global` over a fleet: every session's screen and
//! history searched on demand, nothing indexed.
//!
//! Builds `sessions` terminals (default 30) of agent-transcript-like output
//! at 80 columns, each fed well past the history cap so it holds as much as
//! a real long-running session can, then measures, in a release build:
//!
//! - wall time and rows scanned for a word on no row (the whole fleet's
//!   history must be read), one only on the oldest row of one session, a
//!   common word (each session stops at its line limit) and two words that
//!   must share a row;
//! - peak heap allocated during a search;
//! - how long a keystroke's echo waits for its screen while searches run
//!   back to back, against the same keystrokes with no search.
//!
//! Usage: globalsearchbench [sessions] [rows-per-session] [seconds-of-typing]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use diri_engine::global_search::{SearchScreen, search};
use diri_engine::screen::HeadlessScreen;
use diri_proto::SessionId;
use diri_proto::search::GlobalSearchParams;

struct Counting;
static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let now = CURRENT.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
        PEAK.fetch_max(now, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

const WORDS: &[&str] = &[
    "the",
    "agent",
    "reads",
    "src/main.rs",
    "and",
    "updates",
    "tests",
    "for",
    "parser",
    "build",
    "passes",
    "with",
    "warnings",
    "fixed",
    "function",
    "returns",
    "error",
    "when",
    "input",
    "is",
    "empty",
    "Running",
    "cargo",
    "check",
    "Compiling",
    "diri-engine",
    "v0.1.0",
    "Finished",
];

struct Terminal(Arc<Mutex<HeadlessScreen>>);

impl SearchScreen for Terminal {
    fn screen(&self) -> MutexGuard<'_, HeadlessScreen> {
        self.0.lock().unwrap()
    }
    fn screen_only(&self) -> bool {
        false
    }
}

/// `rows` lines of output; `marker` starts line `marked_row`.
fn terminal(rows: usize, seed: u32, marker: Option<(&str, usize)>) -> HeadlessScreen {
    let mut screen = HeadlessScreen::new(80, 50);
    let mut bytes = Vec::new();
    let mut seed = 0x9e37_79b9_u32 ^ seed;
    let (marker, marked_row) = marker.map_or((None, usize::MAX), |(text, row)| (Some(text), row));
    for row in 0..rows {
        let mut line = String::new();
        if row == marked_row
            && let Some(marker) = marker
        {
            line.push_str(marker);
            line.push(' ');
        }
        while line.len() < 40 + (seed as usize % 35) {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            line.push_str(WORDS[seed as usize % WORDS.len()]);
            line.push(' ');
        }
        bytes.extend_from_slice(line.trim_end().as_bytes());
        bytes.extend_from_slice(b"\r\n");
        if bytes.len() > 1 << 20 {
            screen.feed(&bytes);
            bytes.clear();
        }
    }
    screen.feed(&bytes);
    screen
}

fn ms(duration: Duration) -> String {
    format!("{:.1} ms", duration.as_secs_f64() * 1000.0)
}

fn percentiles(mut samples: Vec<Duration>) -> String {
    samples.sort();
    let at = |q: f64| samples[((samples.len() - 1) as f64 * q) as usize];
    format!(
        "n={} p50={:.1} µs p95={:.1} µs p99={:.1} µs max={:.1} µs",
        samples.len(),
        at(0.5).as_secs_f64() * 1e6,
        at(0.95).as_secs_f64() * 1e6,
        at(0.99).as_secs_f64() * 1e6,
        samples.last().unwrap().as_secs_f64() * 1e6
    )
}

fn params(query: &str, budget_ms: u32) -> GlobalSearchParams {
    GlobalSearchParams {
        query: query.into(),
        max_sessions: 40,
        max_lines_per_session: 6,
        budget_ms,
        screens: true,
        transcripts: false,
        resume: Vec::new(),
    }
}

/// One keystroke echo every millisecond: lock the screen and feed it.
fn typing(screen: &Mutex<HeadlessScreen>, seconds: f64) -> Vec<Duration> {
    let mut samples = Vec::new();
    let end = Instant::now() + Duration::from_secs_f64(seconds);
    while Instant::now() < end {
        let start = Instant::now();
        screen.lock().unwrap().feed(b"x\x08");
        samples.push(start.elapsed());
        std::thread::sleep(Duration::from_millis(1));
    }
    samples
}

fn main() {
    let mut args = std::env::args().skip(1);
    let sessions: usize = args.next().and_then(|v| v.parse().ok()).unwrap_or(30);
    let rows: usize = args.next().and_then(|v| v.parse().ok()).unwrap_or(120_000);
    let typing_seconds: f64 = args.next().and_then(|v| v.parse().ok()).unwrap_or(3.0);

    let built = Instant::now();
    // Output past the history cap is evicted: mark the oldest row a full
    // terminal still holds.
    let oldest_held = rows - terminal(rows, 7, None).total_rows() + 1_000;
    let screens: Vec<(SessionId, Terminal)> = (0..sessions)
        .map(|index| {
            // Session 7 carries "zebra" near its oldest row.
            let marker = (index == 7).then_some(("zebra", oldest_held));
            (
                SessionId::new(format!("s{index}")),
                Terminal(Arc::new(Mutex::new(terminal(rows, index as u32, marker)))),
            )
        })
        .collect();
    let held: usize = screens
        .iter()
        .map(|(_, terminal)| terminal.0.lock().unwrap().total_rows())
        .sum();
    println!(
        "{sessions} sessions, {held} rows held in all ({} per session), built in {}\n",
        held / sessions.max(1),
        ms(built.elapsed())
    );

    println!(
        "| query | first page | first hit | whole fleet (pages) | rows scanned | sessions with hits | peak heap |"
    );
    println!("|---|---|---|---|---|---|---|");
    for (label, query) in [
        ("word on no row", "qwxyz"),
        ("word only on one session's oldest row", "zebra"),
        ("common word", "agent"),
        ("two words on one row", "cargo check"),
    ] {
        let mut runs = Vec::new();
        for _ in 0..3 {
            let baseline = CURRENT.load(Ordering::Relaxed);
            PEAK.store(baseline, Ordering::Relaxed);
            let start = Instant::now();
            let mut request = params(query, 100);
            let mut first_page = None;
            let mut first_hit = None;
            let mut pages = 0;
            let mut rows = 0;
            let mut sessions = std::collections::HashSet::new();
            loop {
                let page = search(&request, &screens, &[]);
                pages += 1;
                rows += page.rows_scanned;
                first_page.get_or_insert(start.elapsed());
                if !page.screens.is_empty() {
                    first_hit.get_or_insert(start.elapsed());
                }
                sessions.extend(page.screens.into_iter().map(|hit| hit.session_id));
                if page.incomplete.resume.is_empty() {
                    break;
                }
                request.resume = page.incomplete.resume;
            }
            let peak = PEAK.load(Ordering::Relaxed).saturating_sub(baseline);
            runs.push((
                first_page.unwrap(),
                first_hit,
                start.elapsed(),
                pages,
                rows,
                sessions.len(),
                peak,
            ));
        }
        runs.sort_by_key(|run| run.2);
        let (first_page, first_hit, whole, pages, rows, sessions, peak) = runs[1];
        println!(
            "| {label} (`{query}`) | {} | {} | {} ({pages}) | {rows} | {sessions} | {:.0} KiB |",
            ms(first_page),
            first_hit.map_or("none".into(), ms),
            ms(whole),
            peak as f64 / 1024.0
        );
    }

    let echo = Arc::clone(&screens[0].1.0);
    println!("\nkeystroke echo wait on one session (lock + feed), {typing_seconds} s each:");
    let idle = typing(&echo, typing_seconds);
    println!("  idle:      {}", percentiles(idle));
    let stop = Arc::new(AtomicBool::new(false));
    let screens = Arc::new(screens);
    let searcher = {
        let stop = Arc::clone(&stop);
        let screens = Arc::clone(&screens);
        std::thread::spawn(move || {
            let mut count = 0;
            while !stop.load(Ordering::Relaxed) {
                search(&params("qwxyz", 250), &screens, &[]);
                count += 1;
            }
            count
        })
    };
    let busy = typing(&echo, typing_seconds);
    stop.store(true, Ordering::Relaxed);
    let count = searcher.join().unwrap();
    println!(
        "  searching: {} ({count} whole-fleet searches back to back)",
        percentiles(busy)
    );
}
