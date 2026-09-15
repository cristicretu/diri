//! A continuously drained preview must survive frames larger than its socket.
#![cfg(unix)]
use diri_engine::attach::AttachHub;
use diri_engine::registry::Registry;
use diri_engine::session::SessionSpec;
use diri_engine::{Authority, ManifestEngine, PtySpec};
use diri_proto::frames::FrameCodec;
use serde_json::json;
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[test]
#[ignore = "child fixture launched by the preview progress regression"]
fn preview_producer_fixture() {
    let Ok(cols) = std::env::var("DIRI_PREVIEW_FIXTURE_COLS") else {
        return;
    };
    let cols: usize = cols.parse().unwrap();
    let rows = if cols == 80 { 24 } else { 50 };
    let deadline = Instant::now() + Duration::from_secs(10);
    while !std::path::Path::new("start").exists() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(2));
    }
    let mut next = Instant::now();
    for sequence in 0..120 {
        let mut bytes = String::new();
        for row in 0..rows {
            bytes.push_str(&format!("\x1b[{};1H", row + 1));
            let start = if row == 0 {
                bytes.push_str(&format!("F{sequence:04} "));
                6
            } else {
                0
            };
            for col in start..cols {
                if col % 12 == 0 {
                    bytes.push_str(&format!(
                        "\x1b[38;5;{}m",
                        20 + (sequence + row + col / 12) % 100
                    ));
                }
                bytes.push(char::from(
                    b'!' + ((row * cols + col + sequence) % 90) as u8,
                ));
            }
        }
        std::io::stdout().write_all(bytes.as_bytes()).unwrap();
        std::io::stdout().flush().unwrap();
        next += Duration::from_micros(16_667);
        if let Some(delay) = next.checked_duration_since(Instant::now()) {
            std::thread::sleep(delay);
        }
    }
    print!("\x1b[1;1HCOMPLETE");
    std::io::stdout().flush().unwrap();
    std::fs::write("completed", "120").unwrap();
    loop {
        std::thread::park();
    }
}

struct Cleanup(Arc<Mutex<Registry>>);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = self.0.lock().unwrap().terminate("fixture", Duration::ZERO);
    }
}

#[test]
fn continuous_preview_drains_large_frames_while_publications_coalesce() {
    for (cols, rows) in [(80, 24), (160, 50)] {
        let temp = tempfile::tempdir().unwrap();
        let (engine, _) =
            ManifestEngine::load_dir(&diri_engine::detect::bundled_manifest_dir()).unwrap();
        let registry = Arc::new(Mutex::new(Registry::new(
            Arc::new(engine),
            temp.path().join("state.json"),
        )));
        let _cleanup = Cleanup(Arc::clone(&registry));
        let record = serde_json::from_value(json!({"id":"fixture","kind":diri_proto::AgentKind::SHELL,"cwd":temp.path(),
            "projectID":"fixture","title":"fixture","titleSource":diri_proto::TitleSource::Placeholder,
            "status":diri_proto::SessionStatus::Idle,"resumability":diri_proto::Resumability::Live,"createdAt":0,"updatedAt":0,"pinned":false})).unwrap();
        registry
            .lock()
            .unwrap()
            .spawn(
                SessionSpec {
                    id: "fixture".into(),
                    pty: PtySpec::new(
                        vec![
                            std::env::current_exe()
                                .unwrap()
                                .to_string_lossy()
                                .into_owned(),
                            "--exact".into(),
                            "preview_producer_fixture".into(),
                            "--ignored".into(),
                            "--nocapture".into(),
                        ],
                        temp.path(),
                    )
                    .size(cols, rows)
                    .env("DIRI_PREVIEW_FIXTURE_COLS", &cols.to_string()),
                    manifest_id: "shell".into(),
                    authority: Authority::ProcessOnly,
                    logs_dir: temp.path().join("logs"),
                    holder: None,
                    remote: None,
                    defer_launch: false,
                },
                record,
            )
            .unwrap();
        let hub = AttachHub::new();
        let (server, client) = UnixStream::pair().unwrap();
        let size: libc::c_int = 8192;
        // Match macOS's default small Unix socket on every test platform.
        // SAFETY: live fixture socket and correctly sized socket option.
        assert_eq!(
            unsafe {
                libc::setsockopt(
                    server.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    (&size as *const libc::c_int).cast(),
                    std::mem::size_of_val(&size) as _,
                )
            },
            0
        );
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let server_registry = Arc::clone(&registry);
        let server_hub = hub.clone();
        let worker = std::thread::spawn(move || {
            let writer = Arc::new(Mutex::new(server.try_clone().unwrap()));
            server_hub.serve_preview(&server_registry, "fixture", server, Vec::new(), writer);
        });
        let mut client = BufReader::new(client);
        let mut ready = String::new();
        client.read_line(&mut ready).unwrap();
        let _: diri_proto::preview::PreviewReady = serde_json::from_str(&ready).unwrap();
        assert!(
            !hub.has_sinks("fixture"),
            "preview must not confer normal attachment visibility"
        );
        std::fs::write(temp.path().join("start"), b"").unwrap();
        let mut codec = FrameCodec::new();
        let mut bytes = [0; 65536];
        let mut seen = HashSet::new();
        let mut complete = false;
        let deadline = Instant::now() + Duration::from_secs(8);
        while !complete && Instant::now() < deadline {
            let count = client.read(&mut bytes).unwrap();
            if count == 0 {
                break;
            }
            for frame in codec.feed(&bytes[..count]).unwrap() {
                if let Some(grid) = frame.grid_payload().unwrap() {
                    for row in grid.changed_rows.iter().filter(|row| row.y == 0) {
                        let text: String = row
                            .cells
                            .iter()
                            .take(8)
                            .filter_map(|cell| char::from_u32(cell.scalar))
                            .collect();
                        complete |= text.starts_with("COMPLETE");
                        if let Some(sequence) = text
                            .strip_prefix('F')
                            .and_then(|text| text.get(..4))
                            .and_then(|text| text.parse::<u32>().ok())
                        {
                            seen.insert(sequence);
                        }
                    }
                }
            }
        }
        while Instant::now() < deadline
            && (!temp.path().join("completed").exists()
                || !registry
                    .lock()
                    .unwrap()
                    .get("fixture")
                    .unwrap()
                    .screen_lines()[0]
                    .starts_with("COMPLETE"))
        {
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(
            std::fs::read_to_string(temp.path().join("completed")).unwrap(),
            "120",
            "producer must finish all frames even if preview fails"
        );
        assert!(
            registry
                .lock()
                .unwrap()
                .get("fixture")
                .unwrap()
                .screen_lines()[0]
                .starts_with("COMPLETE"),
            "Engine must parse the final producer output"
        );
        let _ = client.get_ref().shutdown(std::net::Shutdown::Both);
        worker.join().unwrap();
        assert!(
            complete,
            "{cols}x{rows} preview disconnected before final frame; {} distinct images received while producer and Engine completed 120",
            seen.len()
        );
        assert!(
            seen.len() >= 60,
            "{cols}x{rows} preview must progress throughout output, received {} images",
            seen.len()
        );
    }
}
