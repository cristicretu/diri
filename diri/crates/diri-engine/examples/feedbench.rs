//! Throughput harness for the emulator feed path.
//!
//! `cat` of a large file can finish no faster than the pump drains the PTY, and
//! the pump's per-chunk work is `HeadlessScreen::feed`. This measures that in
//! isolation, against the same payload the terminal-benchmark suite uses, so a
//! change can be judged without rebuilding and restarting the app.
//!
//! Usage: feedbench <file> [cols] [rows]

use diri_engine::screen::HeadlessScreen;
use std::time::Instant;

fn bench(label: &str, data: &[u8], chunk: usize, cols: usize, rows: usize) -> f64 {
    let mut screen = HeadlessScreen::new(cols, rows);
    let start = Instant::now();
    for piece in data.chunks(chunk) {
        screen.feed(piece);
    }
    let elapsed = start.elapsed().as_secs_f64();
    let mb = data.len() as f64 / (1 << 20) as f64;
    println!(
        "{label:<30} {mb:6.1} MB in {:8.1} ms = {:8.1} MB/s   (filled {})",
        elapsed * 1000.0,
        mb / elapsed,
        screen.filled_cells()
    );
    mb / elapsed
}

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: feedbench <file> [cols] [rows]");
    let cols: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(153);
    let rows: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(39);
    let data = std::fs::read(&path).expect("read payload");

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
        );
    }
    // One giant chunk is the ceiling the batching could reach if per-chunk
    // overhead were free.
    bench("feed whole payload", &data, data.len(), cols, rows);
}
