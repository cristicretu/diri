//! Cost of `session.find_history`: Find over a terminal's whole history.
//!
//! For terminals holding 10,000 and ~100,000+ rows of agent-transcript-like
//! output at 80 columns, measures with the app's paging (a short first
//! page, then longer ones):
//!
//! - time to the first match, for a match just below the capture and for
//!   one only on the oldest row (the whole history must be scanned first);
//! - full-search time and page count;
//! - peak heap allocated during the search;
//! - how long a keystroke's echo waits for the screen while a search runs
//!   back to back, against the same keystrokes with no search.
//!
//! Usage: findbench [seconds-of-typing]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use diri_engine::find_history::find_history_page;
use diri_engine::screen::HeadlessScreen;
use diri_proto::SessionId;
use diri_proto::find::FindHistoryParams;

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

/// `rows` lines of prose-like output, with `marker` on the lines `marked`
/// says.
fn transcript(rows: usize, marker: &str, marked: impl Fn(usize) -> bool) -> HeadlessScreen {
    let mut screen = HeadlessScreen::new(80, 50);
    let mut bytes = Vec::new();
    let mut seed = 0x9e37_79b9_u32;
    for row in 0..rows {
        let mut line = String::new();
        if marked(row) {
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

fn thread_cpu() -> Duration {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime fills the provided timespec.
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) };
    Duration::new(time.tv_sec as u64, time.tv_nsec as u32)
}

/// Label, query, and which rows carry the marker.
type Case = (&'static str, &'static str, Box<dyn Fn(usize) -> bool>);

struct Run {
    first_match: Option<Duration>,
    full: Duration,
    cpu: Duration,
    pages: usize,
    matches: usize,
    peak: usize,
}

/// The app's paging: starts below the newest `capture_rows` rows.
fn search(screen: &Mutex<HeadlessScreen>, query: &str, capture_rows: usize) -> Run {
    let (origin, total) = {
        let screen = screen.lock().unwrap();
        (screen.history_origin(), screen.total_rows())
    };
    let mut before = Some(origin + total.saturating_sub(capture_rows) as i64);
    let baseline = CURRENT.load(Ordering::Relaxed);
    PEAK.store(baseline, Ordering::Relaxed);
    let started = Instant::now();
    let cpu = thread_cpu();
    let mut first_match = None;
    let mut pages = 0;
    let mut matches = 0;
    while let Some(row) = before {
        let first = pages == 0;
        let params = FindHistoryParams {
            session_id: SessionId::new("bench"),
            query: query.into(),
            before_row: Some(row),
            cols: Some(80),
            max_matches: if first { 1 } else { 2048 },
            budget_ms: if first { 8 } else { 40 },
        };
        let page = find_history_page(&params, || screen.lock().unwrap()).unwrap();
        pages += 1;
        if first_match.is_none() && !page.matches.is_empty() {
            first_match = Some(started.elapsed());
        }
        matches += page.matches.len();
        before = page.next_before_row;
        if matches >= 10_000 {
            break;
        }
    }
    Run {
        first_match,
        full: started.elapsed(),
        cpu: thread_cpu() - cpu,
        pages,
        matches,
        peak: PEAK.load(Ordering::Relaxed).saturating_sub(baseline),
    }
}

fn ms(duration: Duration) -> String {
    format!("{:.2} ms", duration.as_secs_f64() * 1000.0)
}

fn percentiles(mut samples: Vec<Duration>) -> String {
    samples.sort();
    let at = |q: f64| samples[((samples.len() - 1) as f64 * q) as usize];
    format!(
        "n={} p50={:.1} µs p99={:.1} µs max={:.1} µs",
        samples.len(),
        at(0.5).as_secs_f64() * 1e6,
        at(0.99).as_secs_f64() * 1e6,
        samples.last().unwrap().as_secs_f64() * 1e6
    )
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
    let typing_seconds: f64 = std::env::args()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(3.0);
    // A capture at 80 columns holds 160,000 / 80 = 2,000 rows.
    let capture = diri_proto::FIND_CAPTURE_MAX_CELLS / 80;
    println!(
        "| history rows | case | first match | full search (wall / CPU) | pages | matches | peak heap |"
    );
    println!("|---|---|---|---|---|---|---|");
    for rows in [10_000_usize, 100_000, 200_000] {
        let near = rows.saturating_sub(capture + 100);
        let cases: [Case; 3] = [
            (
                "one match just below the capture",
                "zebra",
                Box::new(move |row| row == near),
            ),
            (
                "one match on the oldest row",
                "zebra",
                Box::new(|row| row == 0),
            ),
            ("common word (cap 10,000)", "agent", Box::new(|_| false)),
        ];
        for (label, query, marked) in cases {
            let screen = Mutex::new(transcript(rows, "zebra", marked));
            let held = screen.lock().unwrap().total_rows();
            let run = search(&screen, query, capture);
            println!(
                "| {held} | {label} | {} | {} / {} | {} | {} | {:.1} KiB |",
                run.first_match.map_or("none".into(), ms),
                ms(run.full),
                ms(run.cpu),
                run.pages,
                run.matches,
                run.peak as f64 / 1024.0
            );
        }
    }

    // One run under the lock, uncontended.
    let mut screen = transcript(100_000, "zebra", |_| false);
    let mut compiled = diri_engine::screen::HistorySearch::new("zebra").unwrap();
    let mut runs = Vec::new();
    let mut before = screen.total_rows();
    let mut found = Vec::new();
    while before > 0 {
        let start = Instant::now();
        before = screen.find_rows_before(
            before,
            diri_proto::find::FIND_HISTORY_CHUNK_ROWS,
            &mut compiled,
            usize::MAX,
            &mut found,
        );
        runs.push(start.elapsed());
    }
    let total: Duration = runs.iter().sum();
    println!(
        "\none run of {} rows under the screen lock: {} (whole history {})",
        diri_proto::find::FIND_HISTORY_CHUNK_ROWS,
        percentiles(runs),
        ms(total)
    );

    // Before: Find captured the newest rows and scanned them in the app.
    let screen = transcript(100_000, "zebra", |_| false);
    let start = Instant::now();
    let cells = screen.find_capture_cells().unwrap();
    println!(
        "\ncapture of the newest {} rows (all Find searched before): {}",
        cells.row_count,
        ms(start.elapsed())
    );

    drop(screen);
    // Typing while a search runs back to back.
    let screen = Arc::new(Mutex::new(transcript(100_000, "zebra", |row| row == 0)));
    println!("\nkeystroke echo wait (lock + feed), {typing_seconds} s each:");
    let idle = typing(&screen, typing_seconds);
    println!("  idle:      {}", percentiles(idle.clone()));
    let stop = Arc::new(AtomicBool::new(false));
    let searcher = {
        let screen = Arc::clone(&screen);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut searches = 0;
            while !stop.load(Ordering::Relaxed) {
                search(&screen, "zebra", capture);
                searches += 1;
            }
            searches
        })
    };
    let busy = typing(&screen, typing_seconds);
    stop.store(true, Ordering::Relaxed);
    let searches = searcher.join().unwrap();
    if let Ok(dir) = std::env::var("FINDBENCH_SAMPLES") {
        // One wait per line in milliseconds, for plotting.
        let write = |name: &str, samples: &[Duration]| {
            let text: String = samples
                .iter()
                .map(|wait| format!("{}\n", wait.as_secs_f64() * 1000.0))
                .collect();
            std::fs::write(std::path::Path::new(&dir).join(name), text).unwrap();
        };
        write("idle.txt", &idle);
        write("searching.txt", &busy);
    }
    println!(
        "  searching: {} ({searches} full searches)",
        percentiles(busy)
    );
}
