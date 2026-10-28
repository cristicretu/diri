//! A line edit reaches an attached client without waiting out the erase
//! hold.
//!
//! Output that removes cells usually means a repaint caught between its
//! erase and its redraw, and the held pump waits up to 16 ms for the redraw
//! so a half-erased frame is never shown. A line editor's own edits remove
//! cells too (a backspace, a history entry shorter than the line it
//! replaces, an autosuggestion the next letter no longer matches, a
//! progress line rewritten shorter), and they have no redraw coming. Before
//! the pump told the two apart, each of those sat out the whole hold.

#![cfg(unix)]

use std::io::{BufRead, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use diri_engine::control::ControlServer;
use diri_engine::detect::ManifestEngine;
use diri_engine::registry::Registry;
use diri_engine::session::HolderConfig;
use diri_proto::ControlMessage;
use diri_proto::frames::{Frame, FrameCodec, FrameType};
use serde_json::json;

/// Well under the 16 ms erase hold the edits used to wait out, and well
/// over what a loaded runner adds to an unheld publication.
const UNHELD_MEDIAN: Duration = Duration::from_millis(9);

/// The Engine's hold on output that erased without its redraw
/// (`OUTPUT_REPAINT_CEILING`), timed from when the Engine read the erase.
const ERASE_HOLD: Duration = Duration::from_millis(16);

struct Engine {
    control: UnixStream,
    socket: PathBuf,
}

fn engine(root: &Path) -> Engine {
    let dir = diri_engine::detect::bundled_manifest_dir()
        .canonicalize()
        .expect("manifests");
    let (manifests, _) = ManifestEngine::load_dir(&dir).expect("load");
    let registry = Arc::new(Mutex::new(Registry::new(
        Arc::new(manifests),
        root.join("state.json"),
    )));
    let server = Arc::new(
        ControlServer::new(Arc::clone(&registry), root.join("daemon.sock"))
            .with_logs_dir(root.join("logs"))
            .with_holder(HolderConfig {
                holders_dir: root.join("holders"),
                executable: PathBuf::from(env!("CARGO_BIN_EXE_diri-holder")),
            }),
    );
    let listener = server.bind().expect("bind");
    let socket = server.socket_path().to_path_buf();
    std::thread::spawn(move || {
        while let Ok((stream, _)) = listener.accept() {
            let server = Arc::clone(&server);
            std::thread::spawn(move || {
                let _ = server.serve(stream);
            });
        }
    });
    Engine {
        control: UnixStream::connect(&socket).expect("connect control"),
        socket,
    }
}

impl Engine {
    fn request(&self, id: u64, method: &str, params: serde_json::Value) -> serde_json::Value {
        let mut bytes = serde_json::to_vec(&ControlMessage::Request {
            id,
            method: method.into(),
            params: Some(params),
        })
        .expect("encode");
        bytes.push(b'\n');
        (&self.control).write_all(&bytes).expect("write");
        let mut reader = std::io::BufReader::new(self.control.try_clone().expect("clone"));
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).expect("reply");
            if let ControlMessage::Response {
                id: reply, result, ..
            } = serde_json::from_str(&line).expect("decode")
                && reply == id
            {
                return result.expect("request succeeded");
            }
        }
    }

    fn spawn(&self, script: &str) -> String {
        let result = self.request(
            1,
            "session.spawn",
            json!({
                "kind": { "shell": {} },
                "cwd": "/tmp",
                "argv": ["/bin/sh", "-c", script],
                "initialCols": 80,
                "initialRows": 24,
            }),
        );
        result["id"].as_str().expect("id").to_string()
    }

    /// A session that prints `first` and then idles, and its terminal opened
    /// for writing: what the test writes there is the child's output, sent
    /// exactly when and in the pieces the test chooses.
    fn spawn_writable(&self, root: &Path, first: &str) -> (String, std::fs::File) {
        let tty_path = root.join("tty");
        let id = self.spawn(&format!(
            "printf '{first}'; tty > '{}'; stty -echo; exec cat",
            tty_path.display()
        ));
        let deadline = Instant::now() + Duration::from_secs(10);
        let tty = loop {
            if let Ok(path) = std::fs::read_to_string(&tty_path)
                && path.ends_with('\n')
            {
                break path.trim().to_string();
            }
            assert!(Instant::now() < deadline, "the child never named its tty");
            std::thread::sleep(Duration::from_millis(20));
        };
        let tty = std::fs::OpenOptions::new()
            .write(true)
            .open(tty)
            .expect("open tty");
        (id, tty)
    }
}

/// An attached data channel that reports when each grid frame arrived and
/// the cursor column it left.
struct Attach {
    stream: UnixStream,
    codec: FrameCodec,
    pending: std::collections::VecDeque<(Instant, u16)>,
}

impl Attach {
    fn open(socket: &Path, id: &str) -> Self {
        let mut stream = UnixStream::connect(socket).expect("connect data");
        let mut attach = serde_json::to_vec(&json!({ "attach": id })).expect("encode");
        attach.push(b'\n');
        stream.write_all(&attach).expect("attach");
        Self {
            stream,
            codec: FrameCodec::new(),
            pending: Default::default(),
        }
    }

    /// The first grid frame whose cursor sits at `column`, waiting at most
    /// `within`.
    fn until_column(&mut self, column: u16, within: Duration) -> Option<Instant> {
        let deadline = Instant::now() + within;
        let mut chunk = [0u8; 64 << 10];
        loop {
            while let Some((at, cursor)) = self.pending.pop_front() {
                if cursor == column {
                    return Some(at);
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            self.stream.set_read_timeout(Some(left)).expect("timeout");
            let count = match self.stream.read(&mut chunk) {
                Ok(0) => panic!("data channel closed"),
                Ok(count) => count,
                Err(_) => return None,
            };
            let at = Instant::now();
            for frame in self.codec.feed(&chunk[..count]).expect("valid frames") {
                if frame.frame_type != FrameType::Grid {
                    continue;
                }
                if let Ok(Some(update)) = frame.grid_payload() {
                    self.pending.push_back((at, update.cursor_col));
                }
            }
        }
    }

    /// Every grid frame published, with when it arrived and its cursor
    /// column, until one sits at `column`.
    fn frames_until(&mut self, column: u16, within: Duration) -> Vec<(Instant, u16)> {
        let deadline = Instant::now() + within;
        let mut seen = Vec::new();
        let mut chunk = [0u8; 64 << 10];
        loop {
            while let Some((at, cursor)) = self.pending.pop_front() {
                seen.push((at, cursor));
                if cursor == column {
                    return seen;
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "never reached column {column}: {seen:?}");
            self.stream.set_read_timeout(Some(left)).expect("timeout");
            let Ok(count) = self.stream.read(&mut chunk) else {
                continue;
            };
            assert!(count > 0, "data channel closed");
            let at = Instant::now();
            for frame in self.codec.feed(&chunk[..count]).expect("valid frames") {
                if frame.frame_type == FrameType::Grid
                    && let Ok(Some(update)) = frame.grid_payload()
                {
                    self.pending.push_back((at, update.cursor_col));
                }
            }
        }
    }

    fn key(&mut self, bytes: &[u8]) {
        self.stream
            .write_all(&FrameCodec::encode(&Frame::input(bytes.to_vec())).unwrap())
            .expect("send key");
    }
}

fn median(mut values: Vec<Duration>) -> Duration {
    values.sort();
    values[values.len() / 2]
}

fn root(name: &str) -> PathBuf {
    // Short: Holder sockets live under it and must fit SUN_LEN.
    let root = PathBuf::from(format!("/tmp/diri-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("root");
    root
}

/// A key that is not an editing key, answered by rewriting the line
/// shorter: Up onto a shorter history entry, or the letter that ends a
/// shell's autosuggestion.
#[test]
fn a_key_answered_by_a_shorter_line_is_published_at_once() {
    let root = root("line-key");
    let engine = engine(&root);
    // A raw-mode line editor: `s` rewrites the line short, anything else
    // rewrites it long. Each answer is one write, as a line editor's is.
    let long = "$ echo a-long-command-line-recalled-from-history";
    let short = "$ true";
    let id = engine.spawn(&format!(
        "printf 'context above the prompt\\r\\n{long}'; stty raw -echo; \
         while k=$(dd bs=1 count=1 2>/dev/null); do \
           if [ \"$k\" = s ]; then printf '\\r\\033[K{short}'; \
           else printf '\\r\\033[K{long}'; fi; done"
    ));
    let mut attach = Attach::open(&engine.socket, &id);
    assert!(
        attach
            .until_column(long.len() as u16, Duration::from_secs(10))
            .is_some(),
        "the editor never drew its line"
    );
    std::thread::sleep(Duration::from_millis(300));
    let mut latencies = Vec::new();
    for _ in 0..15 {
        let sent = Instant::now();
        attach.key(b"s");
        let at = attach
            .until_column(short.len() as u16, Duration::from_secs(2))
            .expect("the shorter line");
        latencies.push(at.duration_since(sent));
        std::thread::sleep(Duration::from_millis(60));
        attach.key(b"l");
        attach
            .until_column(long.len() as u16, Duration::from_secs(2))
            .expect("the longer line");
        std::thread::sleep(Duration::from_millis(60));
    }
    let median = median(latencies.clone());
    assert!(
        median < UNHELD_MEDIAN,
        "a shorter line waited out the erase hold: median {median:?} of {latencies:?}"
    );
    engine.request(2, "session.kill", json!({ "sessionID": id }));
    let _ = std::fs::remove_dir_all(&root);
}

/// Output nobody typed for that edits the cursor's row: a backspace-erase
/// arriving over `ssh`, a progress line rewritten shorter.
#[test]
fn an_unsolicited_line_edit_is_published_at_once() {
    let root = root("line-out");
    let engine = engine(&root);
    let (id, mut tty) = engine.spawn_writable(&root, "context above the line\\r\\n");
    let mut attach = Attach::open(&engine.socket, &id);
    std::thread::sleep(Duration::from_millis(300));
    let mut latencies = Vec::new();
    for round in 0..15u16 {
        // Grow the line, then erase its last character, then rewrite it
        // shorter: the two edits are timed.
        let long = "progress 100 of 100 ...";
        let short = "item 9";
        tty.write_all(format!("\r\x1b[K{long}").as_bytes()).unwrap();
        attach
            .until_column(long.len() as u16, Duration::from_secs(2))
            .expect("the long line");
        std::thread::sleep(Duration::from_millis(40));
        let shorter = format!("\r\x1b[K{short}");
        for (bytes, column) in [
            (&b"\x08 \x08"[..], long.len() - 1),
            (shorter.as_bytes(), short.len()),
        ] {
            let column = column as u16;
            let sent = Instant::now();
            tty.write_all(bytes).unwrap();
            let at = attach
                .until_column(column, Duration::from_secs(2))
                .unwrap_or_else(|| panic!("round {round}: {bytes:?} never published"));
            latencies.push(at.duration_since(sent));
            std::thread::sleep(Duration::from_millis(40));
        }
    }
    let median = median(latencies.clone());
    assert!(
        median < UNHELD_MEDIAN,
        "a line edit waited out the erase hold: median {median:?} of {latencies:?}"
    );
    engine.request(2, "session.kill", json!({ "sessionID": id }));
    let _ = std::fs::remove_dir_all(&root);
}

/// A pager clears its prompt row before drawing the next page, a few ms
/// later. The cursor row cleared to nothing is the start of that redraw, not
/// a line edit, so the empty row must never be published before the erase
/// hold runs out.
///
/// The hold opens when the Engine reads the clear, which is after the test
/// wrote it, so "no empty prompt row sooner than [`ERASE_HOLD`] after the
/// write" holds however slow the runner is; a line edit is published at
/// once. Each round checks it twice: for a clear with no redraw coming (the
/// empty row may only appear once the hold is over), and for a clear and
/// its redraw written apart well inside the hold (normally the empty row is
/// never published; on a runner stalled past the hold it may be, but still
/// not sooner).
#[test]
fn a_prompt_row_cleared_for_its_redraw_is_still_held() {
    let root = root("line-pager");
    let engine = engine(&root);
    let prompt = "lines 1-30 --More--";
    let (id, mut tty) = engine.spawn_writable(&root, &format!("page text above\\r\\n{prompt}"));
    let mut attach = Attach::open(&engine.socket, &id);
    let end = prompt.len() as u16;
    assert!(
        attach.until_column(end, Duration::from_secs(10)).is_some(),
        "the pager never drew its prompt"
    );
    std::thread::sleep(Duration::from_millis(300));
    let clear = b"\r\x1b[K";
    let redraw = format!("next page\r\n{prompt}");
    for round in 0..10 {
        let sent = Instant::now();
        tty.write_all(clear).unwrap();
        let at = attach
            .until_column(0, Duration::from_secs(2))
            .unwrap_or_else(|| panic!("round {round}: the cleared row never published"));
        let held = at.duration_since(sent);
        assert!(
            held >= ERASE_HOLD,
            "round {round}: the cleared prompt row was published after {held:?}"
        );
        tty.write_all(redraw.as_bytes()).unwrap();
        attach
            .until_column(end, Duration::from_secs(2))
            .unwrap_or_else(|| panic!("round {round}: the redraw never published"));
        std::thread::sleep(Duration::from_millis(40));

        let sent = Instant::now();
        tty.write_all(clear).unwrap();
        std::thread::sleep(Duration::from_millis(2));
        tty.write_all(redraw.as_bytes()).unwrap();
        let early: Vec<Duration> = attach
            .frames_until(end, Duration::from_secs(2))
            .into_iter()
            .filter(|&(_, column)| column == 0)
            .map(|(at, _)| at.duration_since(sent))
            .filter(|&after| after < ERASE_HOLD)
            .collect();
        assert!(
            early.is_empty(),
            "round {round}: the cleared prompt row was published alone after {early:?}"
        );
        std::thread::sleep(Duration::from_millis(40));
    }
    engine.request(2, "session.kill", json!({ "sessionID": id }));
    let _ = std::fs::remove_dir_all(&root);
}
