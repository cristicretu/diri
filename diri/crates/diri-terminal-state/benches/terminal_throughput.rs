use std::hint::black_box;
use std::time::{Duration, Instant};

use diri_terminal_state::HeadlessScreen;

const COLS: usize = 160;
const ROWS: usize = 50;
const ITERATIONS: usize = 5_000;
const ROUNDS: usize = 5;

fn measure(mut operation: impl FnMut()) -> Duration {
    let mut rounds = [Duration::ZERO; ROUNDS];
    for elapsed in &mut rounds {
        let started = Instant::now();
        for _ in 0..ITERATIONS {
            operation();
        }
        *elapsed = started.elapsed();
    }
    rounds.sort_unstable();
    rounds[ROUNDS / 2]
}

fn nanos_per_iteration(duration: Duration) -> u128 {
    duration.as_nanos() / ITERATIONS as u128
}

fn budget(name: &str, actual: u128, default: u128) {
    let variable = format!("DIRI_TERM_{}_MAX_NS", name.to_ascii_uppercase());
    let maximum = std::env::var(&variable)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default);
    assert!(
        actual <= maximum,
        "{name} terminal-state cost is {actual} ns/op; budget is {maximum} ({variable})"
    );
}

fn main() {
    // Model a prompt editor: one printable character changes one row and the
    // client asks for a diff after every echo.
    let mut typing = HeadlessScreen::new(COLS, ROWS);
    let _ = typing.grid_update(true);
    let typing_time = measure(|| {
        typing.feed(black_box(b"x\x08"));
        black_box(typing.grid_update(false));
    });

    // Model a full-height build log. Every iteration scrolls the screen once,
    // then constructs the update an attached renderer consumes.
    let mut scrolling = HeadlessScreen::new(COLS, ROWS);
    let line = format!("{}\r\n", "build output ".repeat(12));
    let _ = scrolling.grid_update(true);
    let scrolling_time = measure(|| {
        scrolling.feed(black_box(line.as_bytes()));
        black_box(scrolling.grid_update(false));
    });

    // Cursor-only traffic must remain cheap and must not publish cell rows.
    let mut cursor = HeadlessScreen::new(COLS, ROWS);
    cursor.feed(b"ready");
    let _ = cursor.grid_update(true);
    let cursor_time = measure(|| {
        cursor.feed(black_box(b"\x1b[D\x1b[C"));
        let update = cursor.grid_update(false);
        assert!(update.changed_rows.is_empty());
        black_box(update);
    });

    let typing_ns = nanos_per_iteration(typing_time);
    let scrolling_ns = nanos_per_iteration(scrolling_time);
    let cursor_ns = nanos_per_iteration(cursor_time);
    println!(
        "terminal-state {COLS}x{ROWS}: typing {} ns/op, scrolling {} ns/op, cursor {} ns/op",
        typing_ns, scrolling_ns, cursor_ns,
    );
    budget("typing", typing_ns, 10_000);
    budget("scrolling", scrolling_ns, 100_000);
    budget("cursor", cursor_ns, 10_000);
}
