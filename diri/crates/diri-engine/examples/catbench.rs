//! End-to-end output throughput as a user sees it: `cat` of a large file in a
//! held local session, through the Holder, the Engine and an attached client,
//! until the client's grid shows the file's last line.
//!
//! Everything is private: its own control socket, Holder manager, logs and
//! PTY under a temporary root. The child waits on a FIFO so the clock starts
//! when `cat` does, and reports the moment `cat` returns on a second FIFO, so
//! the drain time (what the writer waits on) and the time to the client's final
//! grid (what the reader waits on) are both measured.
//!
//! The payload must end with a line containing `__BIGLOG_END__`.
//!
//! Usage: catbench <payload> [cols] [rows] [runs]
//!
//! `DIRI_HOLDER_BIN` names a built `diri-holder`. `CATBENCH_THREADS=1` also
//! prints per-thread CPU times of the Engine and Holder processes.

use std::io::{BufRead, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use diri_engine::control::ControlServer;
use diri_engine::detect::ManifestEngine;
use diri_engine::registry::Registry;
use diri_proto::ControlMessage;
use diri_proto::frames::{FrameCodec, FrameType};
use diri_proto::grid::GridCell;
use serde_json::json;

const MARKER: &[u8] = b"__BIGLOG_END__";

fn thread_cpu() -> Duration {
    let mut spec = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: one valid out-pointer.
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut spec) };
    Duration::new(spec.tv_sec as u64, spec.tv_nsec as u32)
}

fn process_cpu() -> Duration {
    // SAFETY: zeroed rusage is a valid out-parameter.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    let tv = |t: libc::timeval| Duration::new(t.tv_sec as u64, t.tv_usec as u32 * 1000);
    tv(usage.ru_utime) + tv(usage.ru_stime)
}

/// CPU seconds of every `diri-holder` descendant of this process.
fn holder_cpu() -> f64 {
    let me = std::process::id().to_string();
    let output = std::process::Command::new("/bin/ps")
        .args(["-A", "-o", "ppid=,pid=,time=,comm="])
        .output()
        .expect("ps");
    let text = String::from_utf8_lossy(&output.stdout);
    let rows: Vec<(String, String, String, String)> = text
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            Some((
                parts.next()?.to_string(),
                parts.next()?.to_string(),
                parts.next()?.to_string(),
                parts.collect::<Vec<_>>().join(" "),
            ))
        })
        .collect();
    let mut ours = vec![me];
    let mut total = 0.0;
    let mut index = 0;
    while index < ours.len() {
        let parent = ours[index].clone();
        for (ppid, pid, time, command) in &rows {
            if *ppid == parent {
                ours.push(pid.clone());
                if command.contains("diri-holder") {
                    total += parse_time(time);
                }
            }
        }
        index += 1;
    }
    total
}

fn parse_time(text: &str) -> f64 {
    // [[dd-]hh:]mm:ss.ss
    text.split(':').fold(0.0, |acc, part| {
        acc * 60.0 + part.parse::<f64>().unwrap_or(0.0)
    })
}

fn row_has_marker(cells: &[GridCell]) -> bool {
    cells.windows(MARKER.len()).any(|window| {
        window
            .iter()
            .zip(MARKER)
            .all(|(c, m)| c.scalar == u32::from(*m))
    })
}

struct Run {
    drained: Duration,
    final_grid: Duration,
    engine_cpu: Duration,
    client_cpu: Duration,
    holder_cpu: f64,
    frames: u64,
    frame_bytes: u64,
}

fn run_once(payload: &Path, cols: u16, rows: u16) -> Run {
    let root = std::path::PathBuf::from(format!("/tmp/dcat-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("root");
    let manifests = diri_engine::detect::bundled_manifest_dir()
        .canonicalize()
        .expect("manifests");
    let (engine, _) = ManifestEngine::load_dir(&manifests).expect("load");
    let registry = Arc::new(Mutex::new(Registry::new(
        Arc::new(engine),
        root.join("state.json"),
    )));
    let holder = std::env::var("DIRI_HOLDER_BIN").expect("set DIRI_HOLDER_BIN");
    let server = Arc::new(
        ControlServer::new(Arc::clone(&registry), root.join("d.sock"))
            .with_logs_dir(root.join("logs"))
            .with_holder(diri_engine::session::HolderConfig {
                holders_dir: root.join("h"),
                executable: holder.into(),
            }),
    );
    let listener = server.bind().expect("bind");
    {
        let server = Arc::clone(&server);
        std::thread::spawn(move || {
            while let Ok((stream, _)) = listener.accept() {
                let server = Arc::clone(&server);
                std::thread::spawn(move || {
                    let _ = server.serve(stream);
                });
            }
        });
    }

    let start_fifo = root.join("start");
    let done_fifo = root.join("done");
    for fifo in [&start_fifo, &done_fifo] {
        let path = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        // SAFETY: valid C string.
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    }

    let control = UnixStream::connect(server.socket_path()).expect("connect control");
    let send = |message: &ControlMessage| {
        let mut bytes = serde_json::to_vec(message).expect("encode");
        bytes.push(b'\n');
        (&control).write_all(&bytes).expect("write");
    };
    send(&ControlMessage::Request {
        id: 1,
        method: "session.spawn".into(),
        params: Some(json!({
            "kind": { "shell": {} },
            "cwd": "/tmp",
            "initialCols": cols,
            "initialRows": rows,
            "argv": [
                "/bin/sh", "-c",
                "read _ < \"$2\"; cat -- \"$1\"; echo done > \"$3\"; exec sleep 600",
                "catbench",
                payload.to_str().unwrap(),
                start_fifo.to_str().unwrap(),
                done_fifo.to_str().unwrap(),
            ],
        })),
    });
    let id = {
        let mut reader = std::io::BufReader::new(control.try_clone().expect("clone"));
        let mut line = String::new();
        reader.read_line(&mut line).expect("spawn reply");
        match serde_json::from_str(&line).expect("decode") {
            ControlMessage::Response {
                result: Ok(result), ..
            } => result["id"].as_str().expect("id").to_string(),
            other => panic!("spawn failed: {other:?}"),
        }
    };
    std::thread::sleep(Duration::from_millis(300));

    let mut data = UnixStream::connect(server.socket_path()).expect("connect data");
    let mut attach = serde_json::to_vec(&json!({ "attach": id })).expect("encode");
    attach.push(b'\n');
    data.write_all(&attach).expect("attach");

    // The client: decode every frame and keep a grid, as the app does.
    let seeded = Arc::new(AtomicBool::new(false));
    let client = {
        let seeded = Arc::clone(&seeded);
        let mut data = data.try_clone().expect("clone");
        std::thread::spawn(move || {
            let mut codec = FrameCodec::new();
            let mut chunk = vec![0u8; 256 << 10];
            let mut grid: Vec<Vec<GridCell>> = Vec::new();
            let mut frames = 0u64;
            let mut bytes = 0u64;
            let mut decode = Duration::ZERO;
            loop {
                let count = data.read(&mut chunk).expect("read frames");
                assert!(count > 0, "data channel closed");
                bytes += count as u64;
                let started = thread_cpu();
                let mut found = false;
                for frame in codec.feed(&chunk[..count]).expect("valid frames") {
                    if frame.frame_type != FrameType::Grid {
                        continue;
                    }
                    frames += 1;
                    let update = frame.grid_payload().expect("decode").expect("grid");
                    if update.is_full_snapshot || grid.len() != usize::from(update.rows) {
                        grid = vec![Vec::new(); usize::from(update.rows)];
                    }
                    seeded.store(true, Ordering::SeqCst);
                    for row in update.changed_rows {
                        let y = usize::from(row.y);
                        if y < grid.len() {
                            found |= row_has_marker(&row.cells);
                            grid[y] = row.cells;
                        }
                    }
                }
                decode += thread_cpu() - started;
                if found {
                    return (Instant::now(), frames, bytes, decode);
                }
            }
        })
    };
    while !seeded.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(5));
    }
    std::thread::sleep(Duration::from_millis(100));

    let holder_before = holder_cpu();
    let cpu_before = process_cpu();
    let done = {
        let done_fifo = done_fifo.clone();
        std::thread::spawn(move || {
            let mut text = String::new();
            std::fs::File::open(&done_fifo)
                .expect("done fifo")
                .read_to_string(&mut text)
                .expect("read done");
            Instant::now()
        })
    };
    let started = Instant::now();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&start_fifo)
        .expect("start fifo")
        .write_all(b"go\n")
        .expect("release");
    let drained_at = done.join().expect("done");
    let (final_at, frames, frame_bytes, client_cpu) = client.join().expect("client");
    let engine_cpu = process_cpu() - cpu_before;
    let holder_after = holder_cpu();
    if std::env::var_os("CATBENCH_THREADS").is_some() {
        let me = std::process::id().to_string();
        let _ = std::process::Command::new("/bin/sh")
            .args([
                "-c",
                &format!("ps -M -p {me} $(pgrep -P {me}) | awk '$NF ~ /:/ || NR==1'"),
            ])
            .status();
    }

    send(&ControlMessage::Request {
        id: 2,
        method: "session.kill".into(),
        params: Some(json!({ "sessionID": id })),
    });
    std::thread::sleep(Duration::from_millis(300));
    drop(data);
    let _ = std::fs::remove_dir_all(&root);
    Run {
        drained: drained_at - started,
        final_grid: final_at - started,
        engine_cpu: engine_cpu.saturating_sub(client_cpu),
        client_cpu,
        holder_cpu: holder_after - holder_before,
        frames,
        frame_bytes,
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let payload =
        std::fs::canonicalize(args.next().expect("usage: catbench <payload>")).expect("payload");
    let cols: u16 = args.next().and_then(|a| a.parse().ok()).unwrap_or(160);
    let rows: u16 = args.next().and_then(|a| a.parse().ok()).unwrap_or(50);
    let runs: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(1);
    let mb = std::fs::metadata(&payload).expect("payload").len() as f64 / 1e6;
    for _ in 0..runs {
        let run = run_once(&payload, cols, rows);
        println!(
            "drain {:.3} s ({:.1} MB/s)  final-grid {:.3} s ({:.1} MB/s)  engine-cpu {:.3} s  \
             client-cpu {:.3} s  holder-cpu {:.2} s  frames {}  frame-MB {:.2}",
            run.drained.as_secs_f64(),
            mb / run.drained.as_secs_f64(),
            run.final_grid.as_secs_f64(),
            mb / run.final_grid.as_secs_f64(),
            run.engine_cpu.as_secs_f64(),
            run.client_cpu.as_secs_f64(),
            run.holder_cpu,
            run.frames,
            run.frame_bytes as f64 / 1e6,
        );
    }
}
