//! Step-1 prototype bench: same print path over the 24-byte `Cell` and the
//! 8-byte packed cell. `cell_proto <payload> ...`
use std::time::Instant;

use alacritty_terminal::cell_proto::{MiniTerm, Packed, ProtoCell, feed};
use alacritty_terminal::term::cell::Cell;
use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};

fn run<C: ProtoCell>(payload: &[u8], reps: usize) -> (f64, String) {
    let mut term = MiniTerm::<C>::new(80, 120, 4 << 20);
    let mut parser = Processor::<StdSyncHandler>::new();
    let start = Instant::now();
    feed(&mut term, &mut parser, payload, 65536, reps);
    let secs = start.elapsed().as_secs_f64();
    ((payload.len() * reps) as f64 / secs / 1e6, term.text())
}

fn median(mut v: Vec<f64>) -> (f64, f64, f64) {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    (v[v.len() / 2], v[0], v[v.len() - 1])
}

fn main() {
    for path in std::env::args().skip(1) {
        let payload = std::fs::read(&path).unwrap();
        let reps = (64 << 20) / payload.len() + 1;
        if std::env::var("PROTO_LOOP").is_ok() {
            loop {
                if std::env::var("PROTO_LOOP").unwrap() == "8" {
                    run::<Packed>(&payload, reps);
                } else {
                    run::<Cell>(&payload, reps);
                }
            }
        }
        // Same final text on both layouts.
        let (_, a) = run::<Cell>(&payload, 1);
        let (_, b) = run::<Packed>(&payload, 1);
        // History holds 3x the rows with 8-byte cells; compare the rows both keep.
        let tail = |s: &str| {
            s.lines()
                .rev()
                .take(80 + (4 << 20) / (120 * 24))
                .collect::<Vec<_>>()
                .join("\n")
        };
        if std::env::var("PROTO_NOCHECK").is_err() {
            assert_eq!(tail(&a), tail(&b), "layouts disagree on {path}");
        }
        let (mut wide, mut packed) = (Vec::new(), Vec::new());
        for _ in 0..7 {
            wide.push(run::<Cell>(&payload, reps).0);
            packed.push(run::<Packed>(&payload, reps).0);
        }
        let (w, wl, wh) = median(wide);
        let (p, pl, ph) = median(packed);
        println!(
            "{path}: 24B {w:.0} ({wl:.0}..{wh:.0})  8B {p:.0} ({pl:.0}..{ph:.0})  x{:.2}",
            p / w
        );
    }
}
