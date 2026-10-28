//! Compares the columns this terminal advances for each string with a
//! reference width, such as Bun's `stringWidth` (Claude Code lays out its
//! screen with it).
//!
//! Input is JSON Lines of `{"cat": "...", "s": "...", "bun": <width>}`.
//! Prints a per-category mismatch table, and with `--list` every mismatch.
//!
//! ```sh
//! cargo run --release -p diri-terminal-state --example width_sweep -- corpus.jsonl [--list]
//! ```

use diri_terminal_state::HeadlessScreen;
use std::collections::BTreeMap;

#[derive(serde::Deserialize)]
struct Entry {
    cat: String,
    s: String,
    bun: usize,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .ok_or("usage: width_sweep corpus.jsonl [--list]")?;
    let list = args.any(|arg| arg == "--list");
    let mut screen = HeadlessScreen::new(400, 4);
    let mut totals: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for line in std::fs::read_to_string(path)?.lines() {
        let entry: Entry = serde_json::from_str(line)?;
        screen.feed(b"\x1b[H\x1b[2K");
        screen.feed(entry.s.as_bytes());
        let (column, _, _) = screen.cursor();
        let width = usize::from(column);
        let total = totals.entry(entry.cat.clone()).or_default();
        total.0 += 1;
        if width != entry.bun {
            total.1 += 1;
            if list {
                let cps: Vec<String> = entry
                    .s
                    .chars()
                    .map(|c| format!("{:04X}", c as u32))
                    .collect();
                println!(
                    "MISMATCH\t{}\t{}\tdiri={width}\tref={}",
                    entry.cat,
                    cps.join(" "),
                    entry.bun
                );
            }
        }
    }
    let (mut all, mut bad) = (0, 0);
    println!("{:<16} {:>7} {:>9}", "category", "cases", "mismatch");
    for (cat, (count, mismatched)) in &totals {
        println!("{cat:<16} {count:>7} {mismatched:>9}");
        all += count;
        bad += mismatched;
    }
    println!("{:<16} {all:>7} {bad:>9}", "total");
    Ok(())
}
