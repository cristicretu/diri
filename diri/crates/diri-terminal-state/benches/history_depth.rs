//! History depth per stored byte: how many rows of realistic output the
//! parser retains, the stored (compressed) bytes per row, and process RSS.
//!
//! Each scenario runs in a fresh child process so RSS is not polluted by an
//! earlier scenario's freed pages. `cargo bench -p diri-terminal-state
//! --bench history_depth` prints one line per content kind and width.
use std::hint::black_box;
use std::process::Command;
use std::time::Instant;

use diri_terminal_state::HeadlessScreen;

/// Lines fed per scenario: more than any depth this probe expects to retain.
const FEED_LINES: usize = 300_000;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

const WORDS: &[&str] = &[
    "the",
    "session",
    "holder",
    "terminal",
    "returns",
    "history",
    "budget",
    "rows",
    "compressed",
    "function",
    "parser",
    "error",
    "test",
    "passes",
    "should",
    "update",
    "config",
    "value",
    "file",
    "Engine",
    "remote",
    "snapshot",
    "cursor",
    "attach",
    "ok",
    "with",
    "from",
    "into",
    "and",
    "of",
];

fn words(rng: &mut Rng, count: usize) -> String {
    let mut out = String::new();
    for i in 0..count {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(WORDS[rng.below(WORDS.len() as u64) as usize]);
    }
    out
}

/// Claude Code-like transcript: truecolor tool headers, dim result lines,
/// box-drawn prompt frames, diffs with full-width tinted backgrounds, CJK
/// prose and wrapped paragraphs.
fn claude(lines: usize, cols: usize) -> Vec<u8> {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut out = Vec::with_capacity(lines * 90);
    let mut emitted = 0;
    while emitted < lines {
        match rng.below(10) {
            0 => {
                let bar = "─".repeat(cols.saturating_sub(2));
                out.extend_from_slice(
                    format!(
                        "\x1b[38;2;136;136;136m╭{bar}╮\x1b[0m\r\n\x1b[38;2;136;136;136m│\x1b[0m > {}\x1b[{}G\x1b[38;2;136;136;136m│\x1b[0m\r\n\x1b[38;2;136;136;136m╰{bar}╯\x1b[0m\r\n",
                        words(&mut rng, 6),
                        cols
                    )
                    .as_bytes(),
                );
                emitted += 3;
            }
            1 | 2 => {
                out.extend_from_slice(
                    format!(
                        "\x1b[38;2;215;119;87m⏺\x1b[0m \x1b[1mRead\x1b[0m(crates/diri-{}/src/{}.rs)\r\n  \x1b[2m⎿  Read {} lines\x1b[0m\r\n",
                        WORDS[rng.below(30) as usize],
                        WORDS[rng.below(30) as usize],
                        rng.below(900)
                    )
                    .as_bytes(),
                );
                emitted += 2;
            }
            3 | 4 => {
                // A diff hunk: tinted full-width rows like an agent's edit view.
                for _ in 0..6 {
                    let (bg, sign) = if rng.below(2) == 0 {
                        ("48;2;2;40;0", '+')
                    } else {
                        ("48;2;61;1;0", '-')
                    };
                    let text = format!(
                        "  {:>4} {sign}    let {} = {}({});",
                        rng.below(2000),
                        WORDS[rng.below(30) as usize],
                        WORDS[rng.below(30) as usize],
                        rng.below(100)
                    );
                    out.extend_from_slice(format!("\x1b[{bg}m{text}\x1b[K\x1b[0m\r\n").as_bytes());
                }
                emitted += 6;
            }
            5 => {
                out.extend_from_slice(
                    format!(
                        "  会话历史已压缩，终端保留了更多的行。{}\r\n",
                        words(&mut rng, 3)
                    )
                    .as_bytes(),
                );
                emitted += 1;
            }
            _ => {
                // Prose that soft-wraps at narrow widths.
                let n = 8 + rng.below(30) as usize;
                out.extend_from_slice(format!("  {}\r\n", words(&mut rng, n)).as_bytes());
                emitted += 1;
            }
        }
    }
    out
}

/// `cargo build` output: bold green verbs, right-aligned, plain crate names.
fn cargo(lines: usize, _cols: usize) -> Vec<u8> {
    let mut rng = Rng(7);
    let mut out = Vec::with_capacity(lines * 60);
    for i in 0..lines {
        let line = match i % 40 {
            39 => format!(
                "\x1b[1m\x1b[33mwarning\x1b[0m\x1b[1m: unused variable: `{}`\x1b[0m\r\n",
                WORDS[rng.below(30) as usize]
            ),
            _ => format!(
                "\x1b[1m\x1b[32m   Compiling\x1b[0m {}-{} v0.{}.{}\r\n",
                WORDS[rng.below(30) as usize],
                WORDS[rng.below(30) as usize],
                rng.below(40),
                rng.below(20)
            ),
        };
        out.extend_from_slice(line.as_bytes());
    }
    out
}

fn seq(lines: usize, _cols: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(lines * 8);
    for i in 1..=lines {
        out.extend_from_slice(format!("{i}\r\n").as_bytes());
    }
    out
}

/// Worst case: every cell a random character in a random truecolor.
fn worst(lines: usize, cols: usize) -> Vec<u8> {
    let mut rng = Rng(42);
    let mut out = Vec::with_capacity(lines * cols * 24);
    for _ in 0..lines {
        for _ in 0..cols {
            let ch = (b'!' + rng.below(94) as u8) as char;
            let (r, g, b) = (rng.below(256), rng.below(256), rng.below(256));
            let (br, bgc, bb) = (rng.below(256), rng.below(256), rng.below(256));
            out.extend_from_slice(
                format!("\x1b[38;2;{r};{g};{b};48;2;{br};{bgc};{bb}m{ch}").as_bytes(),
            );
        }
        out.extend_from_slice(b"\x1b[0m\r\n");
    }
    out
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

fn run(kind: &str, cols: usize) {
    let lines = if kind == "worst" {
        FEED_LINES / 2
    } else {
        FEED_LINES
    };
    let payload = match kind {
        "claude" => claude(lines, cols),
        "cargo" => cargo(lines, cols),
        "seq" => seq(lines, cols),
        "worst" => worst(lines, cols),
        "blank" => b"\r\n".repeat(lines * 4),
        _ => unreachable!(),
    };
    let base = rss_kib();
    let mut screen = HeadlessScreen::new(cols, 50);
    let start = Instant::now();
    for chunk in payload.chunks(64 * 1024) {
        screen.feed(chunk);
    }
    let feed = start.elapsed();
    let mb = payload.len() as f64 / (1 << 20) as f64;
    drop(payload);
    black_box(screen.grid_update(true));
    let rows = screen.history_rows();
    let bytes = screen.history_storage_bytes();
    let rss = rss_kib().saturating_sub(base);
    // Fetch-at-depth: the oldest page, the way scroll-to-top asks for it.
    let start = Instant::now();
    black_box(screen.scrollback_cells(0, 50));
    let oldest = start.elapsed();
    let start = Instant::now();
    black_box(screen.find_capture_cells().ok());
    let find = start.elapsed();
    // A checkpoint samples history under the screen lock.
    let start = Instant::now();
    black_box((screen.history_snapshot(), screen.history_metadata()));
    let checkpoint = start.elapsed();
    // `session.read_scrollback`: every row as text.
    let start = Instant::now();
    black_box(screen.scrollback());
    let text = start.elapsed();
    // A column change (deferred reflow) and back, each with its grid update.
    let start = Instant::now();
    screen.resize(cols - 7, 50);
    black_box(screen.grid_update(false));
    let narrow = start.elapsed();
    let rows_narrow = screen.history_rows();
    let start = Instant::now();
    screen.resize(cols, 50);
    black_box(screen.grid_update(false));
    let widen = start.elapsed();
    let rows_widen = screen.history_rows();
    let start = Instant::now();
    black_box(screen.scrollback_cells(0, 50));
    let oldest_after_resize = start.elapsed();
    let start = Instant::now();
    black_box(screen.full_snapshot());
    let attach = start.elapsed();
    println!(
        "{kind:<6} cols={cols:<3} rows={rows:<7} stored={:>5.2}MiB B/row={:>6.1} rss+={:>5.1}MiB feed={:>5.1}MB/s oldest_page={:>8.2?} find={:>7.2?} checkpoint={:>8.2?} text={:>8.2?} narrow={:>8.2?} widen={:>8.2?} oldest_after={:>8.2?} attach={:>7.2?} rows_narrow={rows_narrow} rows_widen={rows_widen} rows_end={}",
        bytes as f64 / (1 << 20) as f64,
        bytes as f64 / rows.max(1) as f64,
        rss as f64 / 1024.0,
        mb / feed.as_secs_f64(),
        oldest,
        find,
        checkpoint,
        text,
        narrow,
        widen,
        oldest_after_resize,
        attach,
        screen.history_rows(),
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let Some(pos) = args.iter().position(|a| a == "--one") {
        run(&args[pos + 1], args[pos + 2].parse().unwrap());
        return;
    }
    let exe = std::env::current_exe().unwrap();
    for kind in ["claude", "cargo", "seq", "blank", "worst"] {
        for cols in [80, 200] {
            let status = Command::new(&exe)
                .args(["--one", kind, &cols.to_string()])
                .status()
                .unwrap();
            assert!(status.success());
        }
    }
}
