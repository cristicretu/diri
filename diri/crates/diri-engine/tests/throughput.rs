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
use std::sync::Arc;
use std::time::{Duration, Instant};

use diri_engine::session::{HolderConfig, Session, SessionSpec};
use diri_engine::{Authority, ManifestEngine, PtySpec};

/// Payload size. Big enough that per-read overhead shows up over process
/// startup, small enough to stay quick in CI.
const PAYLOAD_BYTES: usize = 8 << 20;
/// Slowest acceptable drain. The path measured ~150 MB/s when this was written;
/// the pre-fix implementation managed ~12 MB/s.
const FLOOR_MB_PER_SEC: f64 = 40.0;

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
         got=b'' if not select.select([fd],[],[],5.0)[0] else os.read(fd,32); \
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
    assert!(
        rate >= FLOOR_MB_PER_SEC,
        "drain fell to {rate:.1} MB/s, below the {FLOOR_MB_PER_SEC:.1} MB/s floor"
    );
}
