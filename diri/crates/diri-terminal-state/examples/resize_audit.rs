//! Manual resize/reflow audit tool, not a test. Runs a program in a PTY,
//! applies a resize sequence to both the PTY and a `HeadlessScreen`, records
//! the byte stream between resizes, and dumps the final state so the same
//! stream can be replayed elsewhere.
//!
//! `resize_audit run <scenario.json> <outdir>` / `resize_audit replay <outdir> <out.json>`
use diri_terminal_state::HeadlessScreen;
use serde::Deserialize;
use std::{
    io::{self, Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Scenario {
    argv: Vec<String>,
    cwd: PathBuf,
    size: [u16; 2],
    #[serde(default)]
    env: Vec<(String, String)>,
    steps: Vec<Step>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Step {
    input: Option<String>,
    size: Option<[u16; 2]>,
    settle_ms: u64,
}

fn dump(screen: &mut HeadlessScreen) -> serde_json::Value {
    let scrollback = screen.scrollback();
    let (col, row, visible) = screen.cursor();
    let (cols, rows) = screen.size();
    serde_json::json!({
        "cols": cols, "rows": rows,
        "cursor": [col, row, visible],
        "history": scrollback.visible_start_row,
        "plain": scrollback.lines,
        "alt": screen.is_alt_screen(),
        "unwrapped": screen.link_source(usize::MAX).text,
        "screen": (0..rows).map(|r| screen.row_text(r)).collect::<Vec<_>>(),
    })
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    match args.first().map(String::as_str) {
        Some("run") if args.len() == 3 => run(Path::new(&args[1]), Path::new(&args[2])),
        Some("replay") if args.len() == 3 => replay(Path::new(&args[1]), Path::new(&args[2])),
        Some("bench") if args.len() == 3 => bench(args[1].parse()?, &args[2]),
        _ => Err("usage: resize_audit run scenario.json outdir | replay outdir out.json".into()),
    }
}

fn replay(dir: &Path, out: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let sizes: Vec<[u16; 2]> = serde_json::from_slice(&std::fs::read(dir.join("sizes.json"))?)?;
    let mut screen = HeadlessScreen::new(sizes[0][0].into(), sizes[0][1].into());
    let mut resize_us = Vec::new();
    for (i, size) in sizes.iter().enumerate() {
        if i > 0 {
            let t = Instant::now();
            screen.resize(size[0].into(), size[1].into());
            resize_us.push(t.elapsed().as_micros() as u64);
        }
        let bytes = std::fs::read(dir.join(format!("seg_{i:04}.bin")))?;
        screen.feed(&bytes);
        let _ = screen.take_replies();
    }
    let mut value = dump(&mut screen);
    value["resizeUs"] = serde_json::json!(resize_us);
    std::fs::write(out, serde_json::to_vec_pretty(&value)?)?;
    Ok(())
}

fn run(scenario: &Path, dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let scenario: Scenario = serde_json::from_slice(&std::fs::read(scenario)?)?;
    std::fs::create_dir_all(dir)?;
    let mut spec = diri_pty::PtySpec::new(scenario.argv, scenario.cwd)
        .size(scenario.size[0], scenario.size[1]);
    spec.env = diri_platform::launch::local_environment();
    spec.env.retain(|(key, _)| {
        !matches!(
            key.to_ascii_uppercase().as_str(),
            "TERM" | "COLORTERM" | "NO_COLOR" | "FORCE_COLOR"
        )
    });
    spec.env.extend([
        ("TERM".into(), "xterm-256color".into()),
        ("COLORTERM".into(), "truecolor".into()),
    ]);
    spec.env.extend(scenario.env);
    let mut pty = diri_pty::Pty::spawn(&spec)?;
    let mut output = pty.reader()?;
    output.set_nonblocking(true)?;
    let mut input = pty.writer()?;
    let mut screen = HeadlessScreen::new(scenario.size[0].into(), scenario.size[1].into());
    let mut sizes = vec![scenario.size];
    let mut segment = Vec::new();
    let mut eof = false;
    let mut resize_us = Vec::new();
    let mut checkpoints = Vec::new();
    for step in scenario.steps {
        if let Some([cols, rows]) = step.size {
            std::fs::write(
                dir.join(format!("seg_{:04}.bin", sizes.len() - 1)),
                &segment,
            )?;
            segment.clear();
            sizes.push([cols, rows]);
            let t = Instant::now();
            screen.resize(cols.into(), rows.into());
            resize_us.push(t.elapsed().as_micros() as u64);
            pty.resize(cols, rows)?;
        }
        if let Some(text) = step.input {
            input.write_all(text.as_bytes())?;
        }
        let deadline = Instant::now() + Duration::from_millis(step.settle_ms);
        let mut buffer = [0; 65536];
        while !eof && Instant::now() < deadline {
            match output.read(&mut buffer) {
                Ok(0) => eof = true,
                Ok(n) => {
                    segment.extend_from_slice(&buffer[..n]);
                    screen.feed(&buffer[..n]);
                    let replies = screen.take_replies();
                    if !replies.is_empty() {
                        input.write_all(&replies)?;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    output.wait_readable(Duration::from_millis(5))?;
                }
                Err(error) if error.raw_os_error() == Some(5) => eof = true,
                Err(error) => return Err(error.into()),
            }
        }
        checkpoints.push(screen.lines());
    }
    std::fs::write(
        dir.join(format!("seg_{:04}.bin", sizes.len() - 1)),
        &segment,
    )?;
    std::fs::write(dir.join("sizes.json"), serde_json::to_vec(&sizes)?)?;
    let mut value = dump(&mut screen);
    value["resizeUs"] = serde_json::json!(resize_us);
    value["checkpoints"] = serde_json::json!(checkpoints);
    std::fs::write(
        dir.join("diri-live.json"),
        serde_json::to_vec_pretty(&value)?,
    )?;
    let _ = pty.terminate(Duration::from_millis(250));
    Ok(())
}

/// Agent-like prose (wrapped at 80) or a build log (mostly unwrapped).
fn content(lines: usize, kind: &str, from: usize) -> Vec<u8> {
    let words = [
        "reflow", "keeps", "every", "soft", "wrapped", "line", "while", "history", "budget",
        "holds",
    ];
    let mut out = Vec::new();
    for i in from..from + lines {
        let line = match kind {
            "prose" => {
                let n = 8 + (i * 7) % 30;
                let body: Vec<&str> = (0..n).map(|k| words[(i + k * 3) % words.len()]).collect();
                format!("  {i:07} {}\r\n", body.join(" "))
            }
            _ => format!(
                "\x1b[3{}m[{i:08}]\x1b[0m Compiling crate_{} v0.{}.0\r\n",
                1 + i % 6,
                i % 997,
                i % 9
            ),
        };
        out.extend_from_slice(line.as_bytes());
    }
    out
}

fn bench(lines: usize, kind: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut screen = HeadlessScreen::new(80, 24);
    screen.feed(&content(lines, kind, 0));
    screen.grid_update(true);
    let history = screen.scrollback_cells(0, 0).live_start_row;
    let step = |screen: &mut HeadlessScreen, c: usize| {
        let t = Instant::now();
        screen.resize(c, 24);
        screen.grid_update(true);
        t.elapsed()
    };
    let mut firsts = Vec::new();
    let mut drags = Vec::new();
    let mut fed = lines;
    for round in 0..5 {
        firsts.push(step(&mut screen, 79));
        for i in 1..=20usize {
            let c = 79 + if i <= 10 { i } else { 20 - i } * 4;
            drags.push(step(&mut screen, c));
        }
        step(&mut screen, 80);
        // Another burst of output before the next drag.
        screen.feed(&content(2_000, kind, fed));
        fed += 2_000;
        screen.grid_update(true);
        let _ = round;
    }
    let mut narrow = Vec::new();
    for c in (40..80).rev().step_by(2).chain((40..=80).step_by(2)) {
        narrow.push(step(&mut screen, c));
    }
    narrow.sort();
    println!(
        "  narrow drag 80->40->80: p50 {:?} p95 {:?} max {:?}; history rows at 80 after: {}",
        narrow[narrow.len() / 2],
        narrow[narrow.len() * 95 / 100],
        narrow[narrow.len() - 1],
        screen.scrollback_cells(0, 0).live_start_row
    );
    drags.sort();
    firsts.sort();
    println!(
        "{kind} {lines} lines -> {history} history rows: first resize after burst p50 {:?} max {:?}; drag step p50 {:?} p95 {:?} max {:?}",
        firsts[firsts.len() / 2],
        firsts[firsts.len() - 1],
        drags[drags.len() / 2],
        drags[drags.len() * 95 / 100],
        drags[drags.len() - 1]
    );
    Ok(())
}
