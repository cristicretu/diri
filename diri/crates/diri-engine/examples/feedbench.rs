//! Throughput harness for the emulator feed path.
//!
//! `cat` of a large file can finish no faster than the pump drains the PTY, and
//! the pump's per-chunk work is `HeadlessScreen::feed`. This measures that in
//! isolation, against the same payload the terminal-benchmark suite uses, so a
//! change can be judged without rebuilding and restarting the app.
//!
//! Usage: feedbench <file> [cols] [rows] [only]
//!
//! `only` runs a single configuration, so whole-process counters such as
//! `/usr/bin/time -l` instructions retired describe it: `4k`, `16k`, `64k`,
//! `whole`, `engine1k`, `engine4k`, `engine64k` or `engine512k`. The `engine`
//! configurations set the screen up as a local Engine session does: a live
//! Holder frame is about a kilobyte, and a pump that is behind reads about
//! 512 KiB of log at a time.

use diri_engine::screen::HeadlessScreen;
use std::time::Instant;

fn bench(label: &str, data: &[u8], chunk: usize, cols: usize, rows: usize, engine: bool) -> f64 {
    // The local Engine also scans for OSC notifications and keeps image
    // anchors; the Remote Helper does neither.
    let mut screen = if engine {
        HeadlessScreen::new(cols, rows)
            .with_notifications()
            .with_inline_images()
            .keeping_spare_rows_until_idle()
    } else {
        HeadlessScreen::new(cols, rows)
    };
    let cpu_start = process_cpu_seconds();
    let start = Instant::now();
    for piece in data.chunks(chunk) {
        screen.feed(piece);
    }
    let elapsed = start.elapsed().as_secs_f64();
    let cpu = process_cpu_seconds() - cpu_start;
    let mb = data.len() as f64 / (1 << 20) as f64;
    // Process CPU time is reported beside wall time: on a busy machine the
    // scheduler inflates wall time, while CPU time tracks the work itself.
    println!(
        "{label:<22} {mb:6.1} MB in {:8.1} ms wall = {:7.1} MB/s, {:8.1} ms CPU = {:7.1} MB/s   (filled {})",
        elapsed * 1000.0,
        mb / elapsed,
        cpu * 1000.0,
        mb / cpu,
        screen.filled_cells()
    );
    mb / cpu
}

fn process_cpu_seconds() -> f64 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: getrusage fills the provided struct; RUSAGE_SELF covers this
    // single-threaded benchmark process.
    let usage = unsafe {
        libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr());
        usage.assume_init()
    };
    let seconds = |time: libc::timeval| time.tv_sec as f64 + time.tv_usec as f64 / 1e6;
    seconds(usage.ru_utime) + seconds(usage.ru_stime)
}

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: feedbench <file> [cols] [rows]");
    let cols: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(153);
    let rows: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(39);
    let only = args.next();
    let data = std::fs::read(&path).expect("read payload");
    if let Some(only) = only {
        let (chunk, engine) = match only.as_str() {
            "4k" => (4 << 10, false),
            "16k" => (16 << 10, false),
            "64k" => (64 << 10, false),
            "whole" => (data.len(), false),
            "engine4k" => (4 << 10, true),
            "engine64k" => (64 << 10, true),
            "engine1k" => (1 << 10, true),
            "engine512k" => (512 << 10, true),
            other => panic!("unknown configuration {other}"),
        };
        bench(&only, &data, chunk, cols, rows, engine);
        return;
    }

    println!(
        "payload {} ({} bytes), grid {cols}x{rows}",
        path,
        data.len()
    );
    for chunk in [4 << 10, 16 << 10, 64 << 10] {
        bench(
            &format!("feed chunk {:>3}K", chunk >> 10),
            &data,
            chunk,
            cols,
            rows,
            false,
        );
    }
    // One giant chunk is the ceiling the batching could reach if per-chunk
    // overhead were free.
    bench("feed whole payload", &data, data.len(), cols, rows, false);
    for chunk in [4 << 10, 64 << 10] {
        bench(
            &format!("engine chunk {:>3}K", chunk >> 10),
            &data,
            chunk,
            cols,
            rows,
            true,
        );
    }
}
