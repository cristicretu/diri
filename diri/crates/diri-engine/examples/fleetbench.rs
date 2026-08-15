//! How the engine holds up with many sessions producing output at once.
//!
//! Every other measurement here has been of one session in isolation, which is
//! not the shape this runs in: a screenful of agents, each with its own PTY,
//! holder, writer thread, subscription and spill file. What this looks for is
//! the cost that only appears in aggregate — threads contending, disks
//! saturating, one loud session starving the others.
//!
//! Reports aggregate throughput, and the spread across sessions, because a
//! fleet that averages well while starving one session is not working.
//!
//! Usage: fleetbench <payload> <sessions> [cols] [rows]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use diri_engine::session::{HolderConfig, Session, SessionSpec};
use diri_engine::{Authority, ManifestEngine, PtySpec};

fn main() {
    let mut args = std::env::args().skip(1);
    let payload = args.next().expect("usage: fleetbench <payload> <sessions>");
    let count: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(8);
    let cols: u16 = args.next().and_then(|a| a.parse().ok()).unwrap_or(153);
    let rows: u16 = args.next().and_then(|a| a.parse().ok()).unwrap_or(39);

    let bytes = std::fs::metadata(&payload).expect("payload").len();
    let root = std::env::temp_dir().join(format!("diri-fleetbench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create dir");

    let manifests = diri_engine::detect::bundled_manifest_dir()
        .canonicalize()
        .expect("manifests");
    let (engine, _) = ManifestEngine::load_dir(&manifests).expect("load");
    let engine = Arc::new(engine);
    let holder_binary = PathBuf::from(
        std::env::var("DIRI_HOLDER_BIN").expect("set DIRI_HOLDER_BIN to a built diri-holder"),
    );

    // Spawned as close together as possible: the point is overlap, so a
    // staggered start would measure something easier than the real thing.
    let started = Instant::now();
    let sessions: Vec<Session> = (0..count)
        .map(|index| {
            let spec = SessionSpec {
                id: format!("s_fleet_{index}"),
                pty: PtySpec::new(
                    vec!["/bin/sh".into(), "-c".into(), format!("cat {payload}")],
                    "/tmp",
                )
                .env("PATH", "/usr/bin:/bin")
                .env("TERM", "xterm-256color")
                .size(cols, rows),
                manifest_id: "shell".into(),
                authority: Authority::ProcessOnly,
                logs_dir: root.join("logs"),
                holder: Some(HolderConfig {
                    holders_dir: root.join("holders"),
                    executable: holder_binary.clone(),
                }),
                remote: None,
                defer_launch: false,
            };
            Session::spawn(spec, Arc::clone(&engine)).expect("spawn")
        })
        .collect();

    // Poll rather than wait per session: finishing times are the measurement,
    // and a thread per session would add its own contention to it.
    let mut finished_at: Vec<Option<Duration>> = vec![None; count];
    let deadline = Instant::now() + Duration::from_secs(600);
    while finished_at.iter().any(Option::is_none) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
        for (index, session) in sessions.iter().enumerate() {
            if finished_at[index].is_none() && session.view().exited {
                finished_at[index] = Some(started.elapsed());
            }
        }
    }

    let mut times: Vec<f64> = finished_at
        .iter()
        .filter_map(|at| at.map(|at| at.as_secs_f64()))
        .collect();
    times.sort_by(f64::total_cmp);
    let unfinished = count - times.len();
    let total_mb = (bytes as f64 / (1 << 20) as f64) * times.len() as f64;
    let wall = times.last().copied().unwrap_or_default();

    println!("sessions           {count} ({unfinished} did not finish)");
    println!(
        "payload each       {:.1} MB",
        bytes as f64 / (1 << 20) as f64
    );
    println!("wall clock         {:.2} s", wall);
    println!(
        "aggregate          {:.1} MB/s",
        total_mb / wall.max(f64::EPSILON)
    );
    if !times.is_empty() {
        // The spread is the fairness question: one session finishing in a
        // tenth of the time of another means the fleet is not sharing.
        println!(
            "per session        fastest {:.2} s / median {:.2} s / slowest {:.2} s",
            times[0],
            times[times.len() / 2],
            times[times.len() - 1]
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}
