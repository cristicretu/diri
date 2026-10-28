//! What idle sessions cost: memory, threads, descriptors and wakeups for the
//! Engine side (this process, which hosts every `Session` and its pump) and
//! the shared Holder manager, plus how fast the first output after a long
//! idle reaches the Engine's screen.
//!
//! Usage: idlebench <sessions> <scrollback-lines> [window-secs] [wake-after-secs]
//!
//! Every session is an interactive `zsh -f`, optionally after printing
//! `scrollback-lines` lines. Once the whole fleet has been quiet for a moment
//! the bench samples both processes, waits `window-secs` (default 60) and
//! samples again: the deltas are the idle cost. With `wake-after-secs`, it
//! then waits until the fleet has been idle that long and types into each
//! session in turn, timing the keystroke echo and a command's output.
//!
//! Set `DIRI_HOLDER_BIN` to a built `diri-holder`. Everything lives in a
//! private temporary directory; nothing touches an installed Diri.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use diri_engine::session::{HolderConfig, Session, SessionSpec};
use diri_engine::{Authority, ManifestEngine, PtySpec};

struct Fleet(Vec<Session>);

impl Drop for Fleet {
    fn drop(&mut self) {
        for session in &mut self.0 {
            if !session.view().exited {
                let _ = session.terminate(Duration::from_secs(2));
            }
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Sample {
    footprint: u64,
    threads: i32,
    fds: i32,
    csw: i32,
    syscalls: i64,
    cpu_ns: u64,
    idle_wakeups: u64,
    interrupt_wakeups: u64,
}

#[cfg(target_os = "macos")]
fn sample(pid: i32) -> Sample {
    let mut out = Sample::default();
    // SAFETY: zeroed out-structs of the sizes the calls are told.
    unsafe {
        let mut usage: libc::rusage_info_v4 = std::mem::zeroed();
        if libc::proc_pid_rusage(
            pid,
            libc::RUSAGE_INFO_V4,
            (&raw mut usage).cast::<libc::rusage_info_t>(),
        ) == 0
        {
            out.footprint = usage.ri_phys_footprint;
            out.idle_wakeups = usage.ri_pkg_idle_wkups;
            out.interrupt_wakeups = usage.ri_interrupt_wkups;
        }
        let mut task: libc::proc_taskinfo = std::mem::zeroed();
        let size = std::mem::size_of::<libc::proc_taskinfo>() as i32;
        if libc::proc_pidinfo(pid, libc::PROC_PIDTASKINFO, 0, (&raw mut task).cast(), size) == size
        {
            out.threads = task.pti_threadnum;
            out.csw = task.pti_csw;
            out.syscalls = i64::from(task.pti_syscalls_unix) + i64::from(task.pti_syscalls_mach);
            // Mach absolute time units; 1 ns on Apple silicon.
            out.cpu_ns = task.pti_total_user + task.pti_total_system;
        }
        let bytes = libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0);
        if bytes > 0 {
            let mut fds = vec![
                std::mem::zeroed::<libc::proc_fdinfo>();
                bytes as usize / std::mem::size_of::<libc::proc_fdinfo>()
            ];
            let used = libc::proc_pidinfo(
                pid,
                libc::PROC_PIDLISTFDS,
                0,
                fds.as_mut_ptr().cast(),
                bytes,
            );
            out.fds = used / std::mem::size_of::<libc::proc_fdinfo>() as i32;
        }
    }
    out
}

#[cfg(not(target_os = "macos"))]
fn sample(_pid: i32) -> Sample {
    Sample::default()
}

fn mb(bytes: u64) -> f64 {
    bytes as f64 / (1 << 20) as f64
}

fn print_point(label: &str, engine: Sample, manager: Sample) {
    println!(
        "{label:<14} engine {:>7.1} MB {:>4} thr {:>4} fd | manager {:>7.1} MB {:>4} thr {:>4} fd",
        mb(engine.footprint),
        engine.threads,
        engine.fds,
        mb(manager.footprint),
        manager.threads,
        manager.fds,
    );
}

fn print_window(label: &str, secs: f64, before: Sample, after: Sample) {
    println!(
        "{label:<8} over {secs:.0}s: csw {:>7}  syscalls {:>8}  cpu {:>8.1} ms  pkg-idle wakeups {:>6}  interrupt wakeups {:>6}",
        after.csw - before.csw,
        after.syscalls - before.syscalls,
        (after.cpu_ns - before.cpu_ns) as f64 / 1e6,
        after.idle_wakeups - before.idle_wakeups,
        after.interrupt_wakeups - before.interrupt_wakeups,
    );
}

fn read_pid(path: &Path) -> i32 {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .unwrap_or(0)
}

fn percentile(values: &mut [f64], p: f64) -> f64 {
    values.sort_by(f64::total_cmp);
    let index = ((values.len() as f64 * p).ceil() as usize).clamp(1, values.len()) - 1;
    values[index]
}

fn wait_until(deadline: Duration, mut done: impl FnMut() -> bool) -> Option<Duration> {
    let started = Instant::now();
    while started.elapsed() < deadline {
        if done() {
            return Some(started.elapsed());
        }
        std::thread::sleep(Duration::from_micros(200));
    }
    None
}

fn main() {
    let mut args = std::env::args().skip(1);
    let usage = "usage: idlebench <sessions> <scrollback-lines> [window-secs] [wake-after-secs]";
    let count: usize = args.next().and_then(|a| a.parse().ok()).expect(usage);
    let lines: usize = args.next().and_then(|a| a.parse().ok()).expect(usage);
    let window: u64 = args.next().and_then(|a| a.parse().ok()).unwrap_or(60);
    let wake_after: Option<u64> = args.next().and_then(|a| a.parse().ok());
    assert!((1..=512).contains(&count));

    let directory = tempfile::Builder::new()
        .prefix("didle-")
        .tempdir_in("/tmp")
        .expect("private root");
    let root = directory.path();
    let manifests = diri_engine::detect::bundled_manifest_dir()
        .canonicalize()
        .expect("manifests");
    let (engine, _) = ManifestEngine::load_dir(&manifests).expect("load");
    let engine = Arc::new(engine);
    let holder_binary = PathBuf::from(
        std::env::var("DIRI_HOLDER_BIN").expect("set DIRI_HOLDER_BIN to a built diri-holder"),
    );
    let holders_dir = root.join("holders");

    let baseline = sample(std::process::id() as i32);
    let mut fleet = Fleet(Vec::with_capacity(count));
    let spawn_started = Instant::now();
    for index in 0..count {
        let script = if lines > 0 {
            format!("seq 1 {lines}; exec /bin/zsh -f -i")
        } else {
            "exec /bin/zsh -f -i".to_string()
        };
        let spec = SessionSpec {
            id: format!("s_idle_{index}"),
            pty: PtySpec::new(
                vec!["/bin/zsh".into(), "-f".into(), "-c".into(), script],
                root.to_string_lossy().into_owned(),
            )
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &root.to_string_lossy())
            .env("TERM", "xterm-256color")
            .size(120, 40),
            manifest_id: "shell".into(),
            authority: Authority::ProcessOnly,
            logs_dir: root.join("logs"),
            holder: Some(HolderConfig {
                holders_dir: holders_dir.clone(),
                executable: holder_binary.clone(),
            }),
            remote: None,
            defer_launch: false,
        };
        fleet
            .0
            .push(Session::spawn(spec, Arc::clone(&engine)).expect("spawn"));
    }
    let manager_pid =
        read_pid(&diri_engine::holder::paths::HolderManagerPaths::new(&holders_dir).pid_file());
    let engine_pid = std::process::id() as i32;

    // Settled: every screen shows a prompt and no session's output moved
    // for two seconds.
    let ready = |session: &Session| {
        session
            .screen_lines()
            .iter()
            .any(|line| line.trim_end().ends_with('%') || line.trim_end().ends_with('#'))
    };
    wait_until(Duration::from_secs(300), || fleet.0.iter().all(ready))
        .expect("every session reaches its prompt");
    let mut tails: Vec<u64> = fleet.0.iter().map(Session::output_tail).collect();
    let mut quiet_since = Instant::now();
    while quiet_since.elapsed() < Duration::from_secs(2) {
        std::thread::sleep(Duration::from_millis(100));
        let now: Vec<u64> = fleet.0.iter().map(Session::output_tail).collect();
        if now != tails {
            tails = now;
            quiet_since = Instant::now();
        }
    }
    let idle_from = quiet_since;
    println!(
        "sessions {count}  scrollback {lines} lines  spawned+settled in {:.1}s  manager pid {manager_pid}",
        spawn_started.elapsed().as_secs_f64()
    );
    print_point("empty engine", baseline, Sample::default());
    let first = (sample(engine_pid), sample(manager_pid));
    print_point(
        &format!("idle {:.0}s", idle_from.elapsed().as_secs_f64()),
        first.0,
        first.1,
    );
    std::thread::sleep(Duration::from_secs(window));
    let second = (sample(engine_pid), sample(manager_pid));
    print_point(
        &format!("idle {:.0}s", idle_from.elapsed().as_secs_f64()),
        second.0,
        second.1,
    );
    // Free memory the allocator still holds dirty is part of the footprint
    // until macOS asks for it back; this shows how much of it is that.
    #[cfg(target_os = "macos")]
    {
        unsafe extern "C" {
            fn malloc_zone_pressure_relief(zone: *mut std::ffi::c_void, goal: usize) -> usize;
        }
        let started = Instant::now();
        // SAFETY: a null zone means every zone; a zero goal means as much as possible.
        let released = unsafe { malloc_zone_pressure_relief(std::ptr::null_mut(), 0) };
        let relieved = sample(engine_pid);
        println!(
            "after relief   engine {:>7.1} MB (allocator released {:.1} MB in {:.2} ms)",
            mb(relieved.footprint),
            mb(released as u64),
            started.elapsed().as_secs_f64() * 1e3
        );
    }
    print_window("engine", window as f64, first.0, second.0);
    print_window("manager", window as f64, first.1, second.1);
    println!(
        "per session    engine {:.2} MB, manager {:.2} MB, engine {:.1} thr, manager {:.1} thr",
        mb(second.0.footprint.saturating_sub(baseline.footprint)) / count as f64,
        mb(second.1.footprint) / count as f64,
        f64::from(second.0.threads - baseline.threads) / count as f64,
        f64::from(second.1.threads) / count as f64,
    );

    if let Some(wake_after) = wake_after {
        let target = Duration::from_secs(wake_after);
        if let Some(rest) = target.checked_sub(idle_from.elapsed()) {
            std::thread::sleep(rest);
        }
        let mut echoes = Vec::with_capacity(count);
        let mut outputs = Vec::with_capacity(count);
        for (index, session) in fleet.0.iter().enumerate() {
            let before = session.screen_lines();
            session.write_input(b"e").expect("type");
            let echo = wait_until(Duration::from_secs(10), || session.screen_lines() != before)
                .expect("keystroke echo");
            let marker = format!("w{index}x");
            session
                .write_input(format!("cho {marker}\r").as_bytes())
                .expect("type");
            let started = Instant::now();
            wait_until(Duration::from_secs(10), || {
                session
                    .screen_lines()
                    .iter()
                    .any(|line| line.trim_end() == marker)
            })
            .expect("command output");
            echoes.push(echo.as_secs_f64() * 1e3);
            outputs.push(started.elapsed().as_secs_f64() * 1e3);
        }
        println!(
            "wake after {:.0}s idle: keystroke echo p50 {:.2} ms p95 {:.2} ms max {:.2} ms | command output p50 {:.2} ms p95 {:.2} ms",
            idle_from.elapsed().as_secs_f64(),
            percentile(&mut echoes.clone(), 0.5),
            percentile(&mut echoes.clone(), 0.95),
            percentile(&mut echoes, 1.0),
            percentile(&mut outputs.clone(), 0.5),
            percentile(&mut outputs, 0.95),
        );
    }
}
