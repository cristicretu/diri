//! Frame rate of a full-screen animation through a real held session.
//!
//! DOOM-fire repaints the whole grid every frame and reports its own rate, so
//! it measures the same drain path as a burst of output but at repaint-shaped
//! chunk sizes. Usage: fpsbench <doom-fire-binary> [cols] [rows]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use diri_engine::session::{HolderConfig, Session, SessionSpec};
use diri_engine::{Authority, ManifestEngine, PtySpec};

fn main() {
    let mut args = std::env::args().skip(1);
    let binary = args.next().expect("usage: fpsbench <doom-fire-binary>");
    let cols: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(153);
    let rows: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(39);
    let frames: String = args.next().unwrap_or_else(|| "1000".into());

    let root = std::env::temp_dir().join(format!("diri-fpsbench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create dir");

    let dir = diri_engine::detect::bundled_manifest_dir()
        .canonicalize()
        .expect("manifests");
    let (engine, _) = ManifestEngine::load_dir(&dir).expect("load");

    let spec = SessionSpec {
        id: "s_fps".into(),
        pty: PtySpec::new(vec![binary], "/tmp")
            .env("PATH", "/usr/bin:/bin")
            .env("TERM", "xterm-256color")
            .env("DOOMFIRE_BENCH", "1")
            .env("DOOMFIRE_WARMUP", "50")
            .env("DOOMFIRE_FRAMES", frames.as_str())
            .size(cols as u16, rows as u16),
        manifest_id: "shell".into(),
        authority: Authority::ProcessOnly,
        logs_dir: root.join("logs"),
        holder: Some(HolderConfig {
            holders_dir: root.join("holders"),
            // Built alongside this example; point DIRI_HOLDER_BIN at it.
            executable: PathBuf::from(
                std::env::var("DIRI_HOLDER_BIN")
                    .expect("set DIRI_HOLDER_BIN to a built diri-holder"),
            ),
        }),
        remote: None,
        defer_launch: false,
    };

    let session = Session::spawn(spec, Arc::new(engine)).expect("spawn");
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
        let tail = session.view().tail_offset;
        let (_, bytes) = session.read_output(tail.saturating_sub(512), 512);
        let text = String::from_utf8_lossy(&bytes);
        if let Some(rest) = text.split("BENCH_FPS").nth(1)
            && let Some(value) = rest.split_whitespace().next()
        {
            println!("{cols}x{rows}: {value} fps");
            return;
        }
    }
    println!("{cols}x{rows}: no result before the deadline");
}
