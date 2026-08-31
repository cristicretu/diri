//! Throughput harness for the PTY→log path.
//!
//! The holder drains the PTY into an `OutputLog`, so a `cat` of a large file
//! can go no faster than `append`. This feeds a fixed payload through in
//! PTY-sized chunks and reports MB/s, with the disk write switched off in a
//! second pass so the in-memory cost can be told apart from the file cost.

use diri_engine::log::{DEFAULT_DISK_CAPACITY, DEFAULT_RING_CAPACITY, OutputLog};
use std::time::Instant;

fn payload(bytes: usize) -> Vec<u8> {
    // Escape-heavy like real terminal output, so the sync-point scan sees work.
    let unit = b"\x1b[38;5;214m\xe2\x96\x80\x1b[0m plain text line of terminal output\n";
    unit.iter().copied().cycle().take(bytes).collect()
}

fn run(label: &str, data: &[u8], chunk: usize, ring: usize, disk: usize) {
    let dir = std::env::temp_dir().join(format!("logbench-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut log = OutputLog::open(&dir, "bench", ring, disk, false).expect("open log");

    let start = Instant::now();
    for piece in data.chunks(chunk) {
        log.append(piece).expect("append");
    }
    let elapsed = start.elapsed();

    let mb = data.len() as f64 / (1 << 20) as f64;
    println!(
        "{label:<28} {mb:6.1} MB in {:7.1} ms = {:7.1} MB/s",
        elapsed.as_secs_f64() * 1000.0,
        mb / elapsed.as_secs_f64()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

fn main() {
    let arg = std::env::args().nth(1);
    // A path replays a real recording; a number generates a synthetic payload.
    let data = match arg {
        Some(a) if std::path::Path::new(&a).exists() => std::fs::read(&a).expect("read payload"),
        Some(a) => payload(a.parse::<usize>().unwrap_or(11 << 20)),
        None => payload(11 << 20),
    };
    // What the holder actually does now: 64 KiB coalesced appends. Vary only
    // the disk cap, to separate steady writing from truncation rewrites.
    for (label, disk) in [
        ("holder cap 32M", 32 << 20),
        ("holder cap 128M", 128 << 20),
        ("holder cap 4G", 4usize << 30),
    ] {
        run(label, &data, 64 << 10, DEFAULT_RING_CAPACITY, disk);
    }

    // The holder reads with a 64 KiB buffer, but a PTY rarely hands over that
    // much at once, so measure a realistic spread of chunk sizes.
    for chunk in [512, 1 << 10, 4 << 10, 16 << 10, 64 << 10] {
        run(
            &format!("chunk {:>3}K default", chunk >> 10),
            &data,
            chunk,
            DEFAULT_RING_CAPACITY,
            DEFAULT_DISK_CAPACITY,
        );
    }
    // Ring capacity is the suspected cost: a front-drain of a Vec is O(capacity)
    // per append, so a bigger ring should be *slower* if that theory holds.
    for ring in [1 << 20, 4 << 20, 16 << 20] {
        run(
            &format!("ring {:>2}M  chunk 16K", ring >> 20),
            &data,
            16 << 10,
            ring,
            DEFAULT_DISK_CAPACITY,
        );
    }
    // Huge disk capacity removes truncate_to_half from the picture.
    run(
        "no truncation  chunk 16K",
        &data,
        16 << 10,
        DEFAULT_RING_CAPACITY,
        1 << 30,
    );
}
