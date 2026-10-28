//! The six `kitten __benchmark__` workloads through the real Holder and
//! Engine: a held session at 120x80 `cat`s each payload (raw tty, so the
//! bytes arrive as written) and the clock stops when the Engine's screen
//! shows the marker printed after it. Every byte is parsed on the way: the
//! marker is the last thing in the stream.
//!
//! ```sh
//! DIRI_VT_BENCH_WRITE=/tmp/vt cargo bench -p diri-terminal-state --bench vt_workloads
//! cargo build --release -p diri-engine --bin diri-holder --example vtbench
//! DIRI_HOLDER_BIN=target/release/diri-holder target/release/examples/vtbench /tmp/vt
//! ```
//!
//! Reported: MB/s (10^6 bytes) over the time from spawn to marker, minus the
//! same for an empty payload, median of 3 runs of at least 64 MB each.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use diri_engine::session::{HolderConfig, Session, SessionSpec};
use diri_engine::{Authority, ManifestEngine, PtySpec};

const MARKER: &str = "VTBENCH-END";
const MIN_BYTES: usize = 64 << 20;
const RUNS: usize = 3;

fn engine() -> Arc<ManifestEngine> {
    let dir = diri_engine::detect::bundled_manifest_dir()
        .canonicalize()
        .expect("manifests");
    let (engine, _) = ManifestEngine::load_dir(&dir).expect("load");
    Arc::new(engine)
}

/// Seconds from spawn until the marker after `payload` is on screen.
fn time_payload(root: &Path, engine: &Arc<ManifestEngine>, payload: &Path, run: usize) -> f64 {
    let holder = HolderConfig {
        holders_dir: root.join("holders"),
        executable: PathBuf::from(
            std::env::var("DIRI_HOLDER_BIN").expect("set DIRI_HOLDER_BIN to a built diri-holder"),
        ),
    };
    let script = format!(
        "stty raw -echo; cat '{}'; printf '\\033[0m\\033[?1049l\\033[2J\\033[H{MARKER}'; exec sleep 600",
        payload.display()
    );
    let spec = SessionSpec {
        id: format!("s_vtbench_{run}"),
        pty: PtySpec::new(vec!["/bin/sh".into(), "-c".into(), script], "/tmp")
            .env("PATH", "/usr/bin:/bin")
            .env("TERM", "xterm-256color")
            .size(120, 80),
        manifest_id: "shell".into(),
        authority: Authority::ProcessOnly,
        logs_dir: root.join("logs"),
        holder: Some(holder),
        remote: None,
        defer_launch: false,
    };
    let started = Instant::now();
    let mut session = Session::spawn(spec, Arc::clone(engine)).expect("spawn held");
    let deadline = started + Duration::from_secs(600);
    while !session
        .screen_lines()
        .iter()
        .any(|line| line.contains(MARKER))
    {
        assert!(Instant::now() < deadline, "the payload never finished");
        std::thread::sleep(Duration::from_micros(500));
    }
    let elapsed = started.elapsed().as_secs_f64();
    let _ = session.terminate(Duration::from_secs(2));
    elapsed
}

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

fn main() {
    let source = PathBuf::from(
        std::env::args()
            .nth(1)
            .expect("usage: vtbench <directory of workload .bin files>"),
    );
    let root = std::env::temp_dir().join(format!("diri-vtbench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("work dir");
    let engine = engine();
    let mut run = 0;
    let mut next = || {
        run += 1;
        run
    };

    let empty = root.join("empty.bin");
    std::fs::write(&empty, b"").expect("empty payload");
    let baseline = median(
        (0..5)
            .map(|_| time_payload(&root, &engine, &empty, next()))
            .collect(),
    );
    println!(
        "spawn-to-marker baseline {:.1} ms (subtracted)",
        baseline * 1e3
    );
    println!("workload            MB/s  Holder->Engine, median of {RUNS}");
    for name in [
        "ascii",
        "unicode",
        "unique_unicode",
        "csi",
        "images",
        "long_escape_codes",
    ] {
        let data = std::fs::read(source.join(format!("{name}.bin")))
            .unwrap_or_else(|error| panic!("{name}.bin: {error}"));
        let reps = MIN_BYTES.div_ceil(data.len());
        let payload = root.join(format!("{name}.bin"));
        std::fs::write(&payload, data.repeat(reps)).expect("payload");
        let bytes = data.len() * reps;
        let seconds = median(
            (0..RUNS)
                .map(|_| time_payload(&root, &engine, &payload, next()) - baseline)
                .collect(),
        );
        println!("{name:<18} {:8.1}", bytes as f64 / seconds / 1e6);
        let _ = std::fs::remove_file(&payload);
    }
    let _ = std::fs::remove_dir_all(&root);
}
