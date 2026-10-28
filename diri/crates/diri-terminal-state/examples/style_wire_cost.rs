//! Wire size and parse/diff cost of terminal styling, before and after a
//! change to what the grid carries. Uses only APIs that predate the styled
//! wire, so the same file measures both sides.
//!
//! `cargo run --release -p diri-terminal-state --example style_wire_cost`

use std::hint::black_box;
use std::time::{Duration, Instant};

use diri_terminal_state::HeadlessScreen;

const COLS: usize = 160;
const ROWS: usize = 50;

fn median(mut operation: impl FnMut(), iterations: usize) -> Duration {
    let mut rounds = [Duration::ZERO; 7];
    for elapsed in &mut rounds {
        let started = Instant::now();
        for _ in 0..iterations {
            operation();
        }
        *elapsed = started.elapsed() / iterations as u32;
    }
    rounds.sort_unstable();
    rounds[rounds.len() / 2]
}

/// A plain build log: no styling at all.
fn plain_screen() -> Vec<u8> {
    let mut bytes = Vec::new();
    for line in 0..ROWS * 3 {
        bytes.extend_from_slice(
            format!("{line:5} compiling diri-terminal-state v0.1.0 (/src/diri) ... ok\r\n")
                .as_bytes(),
        );
    }
    bytes
}

/// An editor full of code with LSP diagnostics: colored undercurls, a dotted
/// spelling hint, a double and a dashed underline, and an overline header,
/// as neovim paints them (`4:3` + `58:2::r:g:b`).
fn styled_screen() -> Vec<u8> {
    let mut bytes = b"\x1b[H\x1b[2J\x1b[53m  src/main.rs  \x1b[55m\r\n".to_vec();
    for line in 1..ROWS {
        bytes.extend_from_slice(format!("\x1b[38;5;244m{line:4} \x1b[0m").as_bytes());
        bytes.extend_from_slice(b"\x1b[38;2;198;120;221mfn\x1b[0m ");
        bytes.extend_from_slice(b"\x1b[38;2;97;175;239mhandle\x1b[0m(");
        if line % 3 == 0 {
            bytes.extend_from_slice(b"\x1b[4:3m\x1b[58:2::224:108:117mrequset\x1b[59m\x1b[4:0m");
        } else {
            bytes.extend_from_slice(b"request");
        }
        bytes.extend_from_slice(b": &Request) -> ");
        if line % 5 == 0 {
            bytes.extend_from_slice(b"\x1b[4:4m\x1b[58;5;3mResult\x1b[59;24m");
        } else if line % 7 == 0 {
            bytes.extend_from_slice(b"\x1b[4:2mResult\x1b[24m \x1b[4:5mdashed\x1b[24m");
        } else {
            bytes.extend_from_slice(b"Result");
        }
        bytes.extend_from_slice(b" { \x1b[4munderlined\x1b[24m }");
        if line + 1 < ROWS {
            bytes.extend_from_slice(b"\r\n");
        }
    }
    bytes
}

fn report(name: &str, input: &[u8]) {
    let mut screen = HeadlessScreen::new(COLS, ROWS);
    screen.feed(input);
    let snapshot = screen.full_snapshot();
    let full_bytes = snapshot.encode().expect("encode").len();
    let encode = median(
        || {
            black_box(black_box(&snapshot).encode().expect("encode"));
        },
        2_000,
    );
    let snapshot_build = median(
        || {
            black_box(screen.full_snapshot());
        },
        2_000,
    );
    // One keystroke into a styled row, diffed and encoded as the pump does.
    let _ = screen.grid_update(true);
    let mut alternate = false;
    let mut diff_bytes = 0;
    let typing = median(
        || {
            alternate = !alternate;
            screen.feed(black_box(if alternate {
                b"\x1b[4;12Hx" as &[u8]
            } else {
                b"\x1b[4;12Hy"
            }));
            let update = screen.grid_update(false);
            diff_bytes = update.encode().expect("encode").len();
            black_box(update);
        },
        20_000,
    );
    // Parse throughput of the same bytes repeated, no diffs.
    let mut corpus = Vec::new();
    while corpus.len() < 8 << 20 {
        corpus.extend_from_slice(input);
    }
    let mut parser = HeadlessScreen::new(COLS, ROWS);
    let started = Instant::now();
    for chunk in corpus.chunks(64 << 10) {
        parser.feed(black_box(chunk));
    }
    let parse = started.elapsed();
    let mb_per_s = corpus.len() as f64 / parse.as_secs_f64() / 1_000_000.0;
    println!(
        "{name:7} full={full_bytes:6} B  diff(1 row)={diff_bytes:4} B  encode={:6.1} us  snapshot={:6.1} us  type+diff+encode={:6.2} us  parse={mb_per_s:6.1} MB/s",
        encode.as_secs_f64() * 1e6,
        snapshot_build.as_secs_f64() * 1e6,
        typing.as_secs_f64() * 1e6,
    );
}

fn main() {
    report("plain", &plain_screen());
    report("styled", &styled_screen());
}
