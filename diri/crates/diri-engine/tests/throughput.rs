//! How fast a session absorbs a burst of output.
//!
//! A `cat` of a large file finishes only as fast as the pump drains the PTY,
//! so this is the number a user feels when a build log or a `cat` of something
//! big scrolls past. It is measured end to end through a real holder, because
//! the costs that matter live in the seam between the read loop and the log,
//! not in either one alone.
//!
//! The floor is deliberately far below what the path actually achieves — it is
//! a guard against a regression that reintroduces per-read work proportional to
//! the retained window, not a target.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use diri_engine::session::{HolderConfig, Session, SessionSpec};
use diri_engine::{Authority, ManifestEngine, PtySpec};

/// Payload size. Big enough that per-read overhead shows up over process
/// startup, small enough to stay quick in CI.
const PAYLOAD_BYTES: usize = 8 << 20;
/// Slowest acceptable drain, checked when `DIRI_PERF_ASSERT` is set.
///
/// Deliberately far below what the path achieves (~90-150 MB/s). It guards the
/// class of regression that made output cost grow with the size of a buffer —
/// which measured ~12 MB/s — not a few percent of tuning.
const FLOOR_MB_PER_SEC: f64 = 25.0;

/// These tests time a pipeline, so they cannot share a machine with each
/// other: cargo runs tests in one binary concurrently, and a burst in the next
/// test over is exactly the interference being measured.
static EXCLUSIVE: Mutex<()> = Mutex::new(());

fn exclusive() -> MutexGuard<'static, ()> {
    EXCLUSIVE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn engine() -> Arc<ManifestEngine> {
    let dir = diri_engine::detect::bundled_manifest_dir()
        .canonicalize()
        .expect("manifests");
    let (engine, _) = ManifestEngine::load_dir(&dir).expect("load");
    Arc::new(engine)
}

fn work_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("diri-throughput-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create dir");
    dir
}

fn payload(dir: &Path) -> PathBuf {
    // Escape-heavy, like real terminal output: a payload of plain ASCII would
    // skip the escape scanning that every chunk pays for in practice.
    let unit = b"\x1b[38;5;214m\xe2\x96\x80\x1b[0m a line of terminal output goes here\n";
    let body: Vec<u8> = unit.iter().copied().cycle().take(PAYLOAD_BYTES).collect();
    let path = dir.join("payload.txt");
    std::fs::write(&path, &body).expect("write payload");
    path
}

fn spec(id: &str, script: &str, logs: &Path, holder: HolderConfig) -> SessionSpec {
    SessionSpec {
        id: id.into(),
        pty: PtySpec::new(vec!["/bin/sh".into(), "-c".into(), script.into()], "/tmp")
            .env("PATH", "/usr/bin:/bin")
            .env("TERM", "xterm-256color")
            .size(153, 39),
        manifest_id: "shell".into(),
        authority: Authority::ProcessOnly,
        logs_dir: logs.to_path_buf(),
        holder: Some(holder),
        remote: None,
        defer_launch: false,
    }
}

/// A program that asks the terminal a question gets an answer.
///
/// `CSI 6n` is the common one: shells, prompt frameworks and TUIs use it to
/// find the cursor column, and a terminal that never replies leaves them
/// blocked on a read until their own timeout fires — or forever.
#[test]
fn a_cursor_position_query_gets_a_reply() {
    let _exclusive = exclusive();
    // Asked immediately, before the pump has drained anything. A shell setting
    // up its prompt does exactly this, and a reply suppressed as "replayed
    // history" until the first empty read leaves it waiting on its own
    // timeout — which is how this read on Linux and not on macOS.
    let root = work_dir("dsr");
    let logs = root.join("logs");
    let holder = HolderConfig {
        holders_dir: root.join("holders"),
        executable: PathBuf::from(env!("CARGO_BIN_EXE_diri-holder")),
    };

    // Ask, read the reply with the terminal in raw mode, then print what came
    // back. A terminal that stays silent yields an empty ANSWER after the
    // timeout rather than hanging the test.
    let script = "python3 -c \"import os,select,sys,termios,tty; \
         fd=os.open('/dev/tty',os.O_RDWR); old=termios.tcgetattr(fd); tty.setraw(fd); \
         os.write(fd,b'\\033[6n'); \
         got=b'' if not select.select([fd],[],[],20.0)[0] else os.read(fd,32); \
         termios.tcsetattr(fd,termios.TCSADRAIN,old); \
         print('ANSWER[%s]' % got.decode('latin1').lstrip('\\033'))\"";
    let session = Session::spawn(spec("s_dsr", script, &logs, holder), engine()).expect("spawn");

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut seen = String::new();
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
        let (_, bytes) = session.read_output(0, 8 << 10);
        seen = String::from_utf8_lossy(&bytes).to_string();
        if seen.contains("ANSWER[") {
            break;
        }
    }

    let answer = seen
        .split("ANSWER[")
        .nth(1)
        .and_then(|rest| rest.split(']').next())
        .unwrap_or_else(|| panic!("no reply to CSI 6n; session output was: {seen:?}"));
    // The reply is CSI <row> ; <col> R, and the ESC was stripped above.
    let (row, col) = answer
        .trim_start_matches('[')
        .trim_end_matches('R')
        .split_once(';')
        .unwrap_or_else(|| panic!("malformed cursor report: {answer:?}"));
    assert!(
        row.parse::<u16>().is_ok() && col.parse::<u16>().is_ok(),
        "cursor report should carry a row and column, got {answer:?}"
    );
}

#[test]
fn a_burst_of_output_drains_at_working_speed() {
    let _exclusive = exclusive();
    let root = work_dir("burst");
    let logs = root.join("logs");
    let file = payload(&root);
    let holder = HolderConfig {
        holders_dir: root.join("holders"),
        executable: PathBuf::from(env!("CARGO_BIN_EXE_diri-holder")),
    };

    // The child times its own `cat`, which is what a user waits on: how long
    // the write side blocks before the shell prompts again. Timing from out
    // here would fold in exit detection, which is a different measurement.
    let script = format!(
        "python3 -c \"import subprocess,time; t=time.perf_counter(); \
         subprocess.run(['cat','{}']); print('ELAPSED_MS', (time.perf_counter()-t)*1000)\"",
        file.display()
    );
    let session = Session::spawn(spec("s_burst", &script, &logs, holder), engine()).expect("spawn");

    let deadline = Instant::now() + Duration::from_secs(60);
    let mut elapsed_ms = None;
    while elapsed_ms.is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
        let tail = session.view().tail_offset;
        // The marker is the last thing written, so only the tail is searched.
        let from = tail.saturating_sub(256);
        let (_, bytes) = session.read_output(from, 256);
        let text = String::from_utf8_lossy(&bytes).to_string();
        elapsed_ms = text
            .split("ELAPSED_MS")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|value| value.trim().parse::<f64>().ok());
    }
    let elapsed_ms = elapsed_ms.expect("cat never reported a time");

    let mb = PAYLOAD_BYTES as f64 / (1 << 20) as f64;
    let rate = mb / (elapsed_ms / 1000.0);
    println!("drained {mb:.1} MB in {elapsed_ms:.1} ms = {rate:.1} MB/s");

    // Measured everywhere, enforced only where the measurement means
    // something. The shared CI runner builds the whole workspace and runs its
    // test binaries concurrently on two cores: the same commit measured 109
    // MB/s in the engine job and 8 MB/s in the workspace job, so no threshold
    // there can tell a regression from a busy neighbour. The number still
    // lands in the log, and `DIRI_PERF_ASSERT=1` turns it into a gate on a
    // machine quiet enough to deserve one.
    if std::env::var_os("DIRI_PERF_ASSERT").is_none() {
        return;
    }
    assert!(
        rate >= FLOOR_MB_PER_SEC,
        "drain fell to {rate:.1} MB/s, below the {FLOOR_MB_PER_SEC:.1} MB/s floor"
    );
}

/// Output reaches the emulator exactly once, in order, however it travelled.
///
/// A held session is fed by two sources: frames the holder pushes as it reads,
/// and the log the daemon tails when no subscription is up. A byte delivered
/// twice, or skipped because each source assumed the other had it, would
/// corrupt the screen rather than fail loudly — so this counts.
///
/// The child prints a numbered line per row of output. Whatever the transport
/// did, the last lines on screen must be the last lines printed, consecutively.
#[test]
fn a_burst_reaches_the_screen_without_gaps_or_repeats() {
    let _exclusive = exclusive();
    const LINES: usize = 200_000;
    let root = work_dir("continuity");
    let logs = root.join("logs");
    let holder = HolderConfig {
        holders_dir: root.join("holders"),
        executable: PathBuf::from(env!("CARGO_BIN_EXE_diri-holder")),
    };

    // seq is faster than a shell loop and its output is trivially checkable.
    let script = format!("seq 1 {LINES}");
    let session =
        Session::spawn(spec("s_continuity", &script, &logs, holder), engine()).expect("spawn");

    let deadline = Instant::now() + Duration::from_secs(120);
    while !session.view().exited && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(session.view().exited, "seq never finished");
    // The pump may still be draining the tail when the child exits.
    let settled = Instant::now() + Duration::from_secs(10);
    let mut numbers = Vec::new();
    while Instant::now() < settled {
        std::thread::sleep(Duration::from_millis(50));
        numbers = session
            .screen_lines()
            .iter()
            .filter_map(|line| line.trim().parse::<usize>().ok())
            .collect();
        if numbers.last() == Some(&LINES) {
            break;
        }
    }

    assert_eq!(
        numbers.last(),
        Some(&LINES),
        "the last line printed should be the last line on screen, got {:?}",
        numbers.last()
    );
    assert!(
        numbers.len() > 4,
        "expected the screen to hold several numbered lines, got {numbers:?}"
    );
    for pair in numbers.windows(2) {
        assert_eq!(
            pair[1],
            pair[0] + 1,
            "screen jumped from {} to {}: output was dropped or repeated",
            pair[0],
            pair[1]
        );
    }
}
