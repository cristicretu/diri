//! A new session's child starts as soon as its size is known, end to end:
//! `session.spawn` on the control socket → the child's first output byte in
//! the Holder log → the first grid frame carrying it on an attachment.
//!
//! The deferred launch used to hold every exec until a pane had reported a
//! size and then for another 120 ms debounce, or for a 400 ms fallback when
//! no pane reported one (CLI and MCP spawns), so a shell tab took ~500 ms to
//! show its prompt. `stty size` is the witness for the launch geometry: the
//! wait existed so an agent's banner renders at the pane's width, and that
//! must still hold.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use diri_engine::control::ControlServer;
use diri_engine::session::HolderConfig;
use diri_engine::{ManifestEngine, OutputLog, Registry};
use diri_proto::ControlMessage;
use diri_proto::frames::{Frame, FrameCodec, FrameType};
use serde_json::{Value, json};

/// Well under the old 400 ms fallback and the 120 ms debounce on top of the
/// pane's report, with room for a Holder start on a loaded CI machine.
const PROMPT_BUDGET: Duration = Duration::from_millis(350);

struct Control {
    writer: UnixStream,
    replies: BufReader<UnixStream>,
    next: u64,
}

impl Control {
    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next += 1;
        let message = ControlMessage::Request {
            id: self.next,
            method: method.into(),
            params: Some(params),
        };
        let mut bytes = serde_json::to_vec(&message).unwrap();
        bytes.push(b'\n');
        self.writer.write_all(&bytes).unwrap();
        let mut line = String::new();
        self.replies.read_line(&mut line).unwrap();
        match serde_json::from_str::<ControlMessage>(&line).unwrap() {
            ControlMessage::Response {
                result: Ok(result), ..
            } => result,
            other => panic!("{method}: unexpected {other:?}"),
        }
    }
}

struct Engine {
    _registry: Arc<Mutex<Registry>>,
    server: Arc<ControlServer>,
    control: Control,
    logs: std::path::PathBuf,
}

/// A held-transport Engine on a private socket, as the app runs it.
fn start(temp: &Path) -> Engine {
    let (engine, _) =
        ManifestEngine::load_dir(&diri_engine::detect::bundled_manifest_dir()).unwrap();
    let registry = Arc::new(Mutex::new(Registry::new(
        Arc::new(engine),
        temp.join("state.json"),
    )));
    let server = Arc::new(
        ControlServer::new(Arc::clone(&registry), temp.join("daemon.sock"))
            .with_logs_dir(temp.join("logs"))
            .with_holder(HolderConfig {
                holders_dir: temp.join("holders"),
                executable: env!("CARGO_BIN_EXE_diri-holder").into(),
            }),
    );
    let listener = server.bind().unwrap();
    let serving = Arc::clone(&server);
    std::thread::spawn(move || {
        while let Ok((stream, _)) = listener.accept() {
            let server = Arc::clone(&serving);
            std::thread::spawn(move || {
                let _ = server.serve(stream);
            });
        }
    });
    let control = UnixStream::connect(server.socket_path()).unwrap();
    control
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let writer = control.try_clone().unwrap();
    Engine {
        _registry: registry,
        server,
        control: Control {
            writer,
            replies: BufReader::new(control),
            next: 0,
        },
        logs: temp.join("logs"),
    }
}

/// The App's data channel to one session.
struct Attachment {
    stream: UnixStream,
    codec: FrameCodec,
    text: String,
}

impl Attachment {
    fn open(engine: &Engine, id: &str) -> Self {
        let mut stream = UnixStream::connect(engine.server.socket_path()).unwrap();
        let mut line = serde_json::to_vec(&json!({ "attach": id })).unwrap();
        line.push(b'\n');
        stream.write_all(&line).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(5)))
            .unwrap();
        Self {
            stream,
            codec: FrameCodec::new(),
            text: String::new(),
        }
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        self.stream
            .write_all(&FrameCodec::encode(&Frame::resize(cols, rows)).unwrap())
            .unwrap();
    }

    /// Reads grid frames until the screen shows `needle`; returns when.
    fn until_grid_shows(&mut self, needle: &str) -> Instant {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut chunk = [0u8; 64 << 10];
        loop {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {needle:?} on the grid; saw {:?}",
                self.text
            );
            let count = match self.stream.read(&mut chunk) {
                Ok(0) => panic!("data channel closed waiting for {needle:?}"),
                Ok(count) => count,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    continue;
                }
                Err(error) => panic!("read frames: {error}"),
            };
            for frame in self.codec.feed(&chunk[..count]).unwrap() {
                if frame.frame_type != FrameType::Grid {
                    continue;
                }
                let Ok(Some(update)) = frame.grid_payload() else {
                    continue;
                };
                for row in &update.changed_rows {
                    let line: String = row
                        .cells
                        .iter()
                        .map(|cell| char::from_u32(cell.scalar).unwrap_or(' '))
                        .collect();
                    self.text.push_str(line.trim_end());
                    self.text.push('\n');
                }
                if self.text.contains(needle) {
                    return Instant::now();
                }
            }
        }
    }
}

fn log_text(logs: &Path, id: &str) -> String {
    let Ok(mut log) = OutputLog::reader(logs, id) else {
        return String::new();
    };
    log.refresh_from_disk();
    let tail = log.tail_offset();
    let (_, bytes) = log.read(0, tail as usize);
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Polls the Holder log for the child's first byte; returns when it landed.
fn first_output(logs: &Path, id: &str) -> Instant {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if !log_text(logs, id).is_empty() {
            return Instant::now();
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    panic!("{id} never produced output");
}

fn spawn(engine: &mut Engine, script: &str, size: Option<(u16, u16)>) -> (String, Instant) {
    let mut params = json!({
        "kind": { "shell": {} },
        "cwd": "/tmp",
        "argv": ["/bin/sh", "-c", script],
    });
    if let Some((cols, rows)) = size {
        params["initialCols"] = json!(cols);
        params["initialRows"] = json!(rows);
    }
    let requested = Instant::now();
    let spawned = engine.control.request("session.spawn", params);
    (spawned["id"].as_str().unwrap().to_owned(), requested)
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

/// The App names the pane's size in the request: the child starts at once,
/// at exactly that size, and its first output reaches an attachment well
/// inside the old fallback window.
#[test]
fn a_spawn_that_names_its_size_starts_at_once_at_that_size() {
    let temp = tempfile::tempdir().unwrap();
    let mut engine = start(temp.path());

    let (id, requested) = spawn(&mut engine, "stty size; exec cat", Some((101, 31)));
    let mut pane = Attachment::open(&engine, &id);
    let output = first_output(&engine.logs, &id);
    let grid = pane.until_grid_shows("31 101");
    eprintln!(
        "named size: first output {:.1} ms, first grid frame {:.1} ms",
        ms(output - requested),
        ms(grid - requested)
    );
    assert!(
        grid - requested < PROMPT_BUDGET,
        "the child waited for a size the request already named: {:.1} ms",
        ms(grid - requested)
    );
    assert!(
        !log_text(&engine.logs, &id).contains("24 80"),
        "the child must start at the named size, never at the 80x24 default"
    );
    engine
        .control
        .request("session.kill", json!({ "sessionID": id }));
}

/// A CLI or MCP spawn names no size. Once any pane has reported one, the
/// next session starts at it at once instead of waiting out the fallback.
#[test]
fn a_spawn_without_a_size_starts_at_the_size_a_pane_last_reported() {
    let temp = tempfile::tempdir().unwrap();
    let mut engine = start(temp.path());

    let (first, _) = spawn(
        &mut engine,
        "trap 'stty size' WINCH; stty size; while :; do sleep 0.02; done",
        Some((80, 24)),
    );
    let mut pane = Attachment::open(&engine, &first);
    pane.until_grid_shows("24 80");
    pane.resize(133, 37);
    pane.until_grid_shows("37 133");

    let (id, requested) = spawn(&mut engine, "stty size; exec cat", None);
    let output = first_output(&engine.logs, &id);
    let mut viewer = Attachment::open(&engine, &id);
    let grid = viewer.until_grid_shows("37 133");
    eprintln!(
        "CLI spawn: first output {:.1} ms, first grid frame {:.1} ms",
        ms(output - requested),
        ms(grid - requested)
    );
    assert!(
        output - requested < PROMPT_BUDGET,
        "a CLI spawn waited out the launch fallback: {:.1} ms",
        ms(output - requested)
    );
    for id in [first, id] {
        engine
            .control
            .request("session.kill", json!({ "sessionID": id }));
    }
}

/// With no size known anywhere, the pane's first report launches the child
/// at once, at that size: no debounce after it.
#[test]
fn the_first_pane_size_launches_at_once_when_nothing_is_known() {
    let temp = tempfile::tempdir().unwrap();
    let mut engine = start(temp.path());

    let (id, requested) = spawn(&mut engine, "stty size; exec cat", None);
    let mut pane = Attachment::open(&engine, &id);
    pane.resize(97, 29);
    let reported = Instant::now();
    let grid = pane.until_grid_shows("29 97");
    eprintln!(
        "first pane size: first grid frame {:.1} ms after the request, {:.1} ms after the size",
        ms(grid - requested),
        ms(grid - reported)
    );
    assert!(
        !log_text(&engine.logs, &id).contains("24 80"),
        "the child must start at the pane's size, not the default"
    );
    // No debounce after the report is pinned without wall-clock noise by
    // the unit test `a_client_size_ends_the_launch_wait_at_once`.
    assert!(
        grid - requested < PROMPT_BUDGET,
        "the launch waited after the pane reported its size: {:.1} ms",
        ms(grid - requested)
    );
    engine
        .control
        .request("session.kill", json!({ "sessionID": id }));
}

/// A pane whose size differs from the one the child started at resizes the
/// child like any later resize, so a banner drawn after it uses the pane's
/// width (an agent boots for hundreds of milliseconds before drawing).
#[test]
fn a_pane_size_that_differs_from_the_launch_size_reaches_the_child() {
    let temp = tempfile::tempdir().unwrap();
    let mut engine = start(temp.path());

    let (id, _) = spawn(
        &mut engine,
        "sleep 0.3; echo drawn at $(stty size); exec cat",
        Some((120, 40)),
    );
    let mut pane = Attachment::open(&engine, &id);
    pane.resize(90, 28);
    pane.until_grid_shows("drawn at 28 90");
    engine
        .control
        .request("session.kill", json!({ "sessionID": id }));
}
