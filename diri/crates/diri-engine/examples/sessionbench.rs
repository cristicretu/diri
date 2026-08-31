//! The parts of a session's life that are not throughput.
//!
//! Draining output fast is one thing a terminal is judged on; these are the
//! others, and they are the ones a user feels as "sluggish" rather than
//! "slow": how long a new session takes to become usable, how long switching
//! to an existing one takes to show its screen, what a window drag costs, and
//! what a fleet of idle sessions costs when nobody is doing anything at all.
//!
//! Usage: sessionbench <startup|attach|resize|idle> [count]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use diri_engine::holder::HolderClient;
use diri_engine::session::{HolderConfig, Session, SessionSpec};
use diri_engine::{Authority, ManifestEngine, PtySpec};

fn engine() -> Arc<ManifestEngine> {
    let dir = diri_engine::detect::bundled_manifest_dir()
        .canonicalize()
        .expect("manifests");
    let (engine, _) = ManifestEngine::load_dir(&dir).expect("load");
    Arc::new(engine)
}

fn work_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("diri-sessionbench-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create dir");
    dir
}

fn holder_config(root: &Path) -> HolderConfig {
    HolderConfig {
        holders_dir: root.join("holders"),
        executable: PathBuf::from(
            std::env::var("DIRI_HOLDER_BIN").expect("set DIRI_HOLDER_BIN to a built diri-holder"),
        ),
    }
}

fn spec(id: &str, script: &str, root: &Path, cols: u16, rows: u16) -> SessionSpec {
    SessionSpec {
        id: id.into(),
        pty: PtySpec::new(vec!["/bin/sh".into(), "-c".into(), script.into()], "/tmp")
            .env("PATH", "/usr/bin:/bin")
            .env("TERM", "xterm-256color")
            .size(cols, rows),
        manifest_id: "shell".into(),
        authority: Authority::ProcessOnly,
        logs_dir: root.join("logs"),
        holder: Some(holder_config(root)),
        remote: None,
        defer_launch: false,
    }
}

fn percentiles(mut samples: Vec<f64>, label: &str) {
    if samples.is_empty() {
        println!("{label}: no samples");
        return;
    }
    samples.sort_by(f64::total_cmp);
    let at = |q: f64| samples[((samples.len() as f64 - 1.0) * q).round() as usize];
    println!(
        "{label:<22} median {:.1} ms   p95 {:.1} ms   worst {:.1} ms   (n={})",
        at(0.5),
        at(0.95),
        at(1.0),
        samples.len()
    );
}

/// Time from spawning a session to its first prompt being on screen.
///
/// A shell prints its prompt as soon as it is ready, so this is the whole
/// startup path: process spawn, PTY, holder, first output, emulator, screen.
fn startup(count: usize) {
    let root = work_dir("startup");
    let mut samples = Vec::new();
    // Split, because the two halves have different owners: getting a holder
    // and a PTY, versus the first output finding its way to a screen.
    let mut spawn_samples = Vec::new();
    for index in 0..count {
        let started = Instant::now();
        let mut session = Session::spawn(
            spec(
                &format!("s_start_{index}"),
                "printf 'READY> '; sleep 30",
                &root,
                153,
                39,
            ),
            engine(),
        )
        .expect("spawn");
        spawn_samples.push(started.elapsed().as_secs_f64() * 1000.0);
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if session
                .screen_lines()
                .iter()
                .any(|line| line.contains("READY>"))
            {
                samples.push(started.elapsed().as_secs_f64() * 1000.0);
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        let _ = session.terminate(Duration::from_secs(2));
    }
    percentiles(spawn_samples, "  of which spawn");
    percentiles(samples, "startup to prompt");
    let _ = std::fs::remove_dir_all(&root);
}

/// Time from adopting a live session to its screen being restored.
///
/// This is what switching to a session that a previous daemon was running
/// costs: the scrollback has to be replayed through a fresh emulator before
/// anything can be shown.
fn attach(count: usize) {
    let root = work_dir("attach");
    // A screenful of history to restore, with a marker the check can find.
    let script = "for i in $(seq 1 4000); do echo \"history line $i\"; done; \
         printf 'ATTACHME\\n'; sleep 120";
    let session =
        Session::spawn(spec("s_attach", script, &root, 153, 39), engine()).expect("spawn");
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if session
            .screen_lines()
            .iter()
            .any(|line| line.contains("ATTACHME"))
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    // The holder outlives this; every adoption below is a fresh daemon
    // meeting a session that is already running.
    drop(session);

    let holder = holder_config(&root);
    let paths = diri_engine::holder::HolderPaths::new(&holder.holders_dir, "s_attach");
    let client = HolderClient::new(paths.socket());
    let mut samples = Vec::new();
    for _ in 0..count {
        let stat = client.stat().expect("holder alive");
        let started = Instant::now();
        let adopted = Session::adopt(
            spec("s_attach", "", &root, 153, 39),
            &holder,
            &stat,
            engine(),
        )
        .expect("adopt");
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            if adopted
                .screen_lines()
                .iter()
                .any(|line| line.contains("ATTACHME"))
            {
                samples.push(started.elapsed().as_secs_f64() * 1000.0);
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        drop(adopted);
    }
    percentiles(samples, "attach to screen");
    let _ = client.kill_tree();
    let _ = std::fs::remove_dir_all(&root);
}

/// What one step of a window drag costs.
///
/// A drag is not one resize, it is a stream of them, and each reflows the
/// grid. Measured as the round trip to a screen that has taken the new size.
fn resize(count: usize) {
    let root = work_dir("resize");
    let script = "for i in $(seq 1 2000); do echo \"a line of scrollback $i\"; done; sleep 120";
    let mut session =
        Session::spawn(spec("s_resize", script, &root, 153, 39), engine()).expect("spawn");
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline && session.screen_lines().is_empty() {
        std::thread::sleep(Duration::from_millis(5));
    }

    let mut samples = Vec::new();
    for step in 0..count {
        // Widths a drag would actually pass through, not one repeated size.
        let cols = 80 + (step as u16 % 100);
        let started = Instant::now();
        session.resize(cols, 39).expect("resize");
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if session.screen_size().0 == cols as usize {
                samples.push(started.elapsed().as_secs_f64() * 1000.0);
                break;
            }
            std::thread::sleep(Duration::from_micros(200));
        }
    }
    percentiles(samples, "resize step");
    let _ = session.terminate(Duration::from_secs(2));
    let _ = std::fs::remove_dir_all(&root);
}

/// What a fleet of idle sessions costs when nothing is happening.
///
/// Every session holds a PTY, a holder, threads and a subscription. Idle cost
/// is a battery question, and the only honest way to ask it is to leave them
/// alone and watch.
fn idle(count: usize) {
    let root = work_dir("idle");
    let sessions: Vec<Session> = (0..count)
        .map(|index| {
            Session::spawn(
                spec(&format!("s_idle_{index}"), "sleep 600", &root, 153, 39),
                engine(),
            )
            .expect("spawn")
        })
        .collect();
    // Let startup settle before measuring what steady state costs.
    std::thread::sleep(Duration::from_secs(5));

    let sample_secs = 10;
    let before = cpu_seconds();
    std::thread::sleep(Duration::from_secs(sample_secs));
    let used = cpu_seconds() - before;
    println!(
        "idle {count} sessions      {:.2}% of one core over {sample_secs}s ({:.1} ms per session per second)",
        used / sample_secs as f64 * 100.0,
        used * 1000.0 / sample_secs as f64 / count.max(1) as f64
    );
    for mut session in sessions {
        let _ = session.terminate(Duration::from_secs(2));
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// CPU seconds this process and its children have used.
fn cpu_seconds() -> f64 {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    let mut total = 0.0;
    for who in [libc::RUSAGE_SELF, libc::RUSAGE_CHILDREN] {
        // SAFETY: `usage` is a valid, fully initialized rusage.
        if unsafe { libc::getrusage(who, &mut usage) } == 0 {
            total += usage.ru_utime.tv_sec as f64 + usage.ru_utime.tv_usec as f64 / 1e6;
            total += usage.ru_stime.tv_sec as f64 + usage.ru_stime.tv_usec as f64 / 1e6;
        }
    }
    total
}

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "startup".into());
    let count: usize = std::env::args()
        .nth(2)
        .and_then(|a| a.parse().ok())
        .unwrap_or(20);
    match mode.as_str() {
        "startup" => startup(count),
        "attach" => attach(count),
        "resize" => resize(count),
        "idle" => idle(count),
        other => eprintln!("unknown mode {other}: expected startup, attach, resize or idle"),
    }
}
