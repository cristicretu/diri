//! An old live Holder must not be renegotiated on every log wakeup.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use diri_engine::holder::{HolderPaths, protocol::HolderStat};
use diri_engine::session::{HolderConfig, Session, SessionSpec};
use diri_engine::{Authority, ManifestEngine, OutputLog, PtySpec};

struct OldHolder {
    stop: Arc<AtomicBool>,
    attempts: Arc<AtomicUsize>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Drop for OldHolder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let result = self.worker.take().unwrap().join();
        if !std::thread::panicking() {
            result.unwrap();
        }
    }
}

fn wait_for(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !condition() {
        assert!(Instant::now() < deadline, "condition timed out");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn old_holder_is_negotiated_once_while_log_output_keeps_arriving() {
    exercise_old_holder(false);
}

#[test]
fn failed_transport_is_retried_before_pinning_old_holder_capabilities() {
    exercise_old_holder(true);
}

fn exercise_old_holder(fail_first: bool) {
    let root = tempfile::tempdir_in("/tmp").unwrap();
    let config = HolderConfig {
        holders_dir: root.path().join("holders"),
        executable: "/unused".into(),
    };
    let paths = HolderPaths::new(&config.holders_dir, "s_compat");
    std::fs::create_dir_all(&paths.directory).unwrap();
    let listener = UnixListener::bind(paths.socket()).unwrap();
    listener.set_nonblocking(true).unwrap();
    let stat = HolderStat {
        child_pid: std::process::id() as i32,
        alive: true,
        log_offset: 0,
        foreground_pid: None,
        cols: Some(80),
        rows: Some(24),
        epoch_offset: Some(0),
    };
    let stop = Arc::new(AtomicBool::new(false));
    let attempts = Arc::new(AtomicUsize::new(0));
    let worker = {
        let stop = stop.clone();
        let attempts = attempts.clone();
        let stat = stat.clone();
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let (mut stream, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    Err(e) => panic!("accept: {e}"),
                };
                let request = read_request(&mut stream);
                let response = match request["op"].as_str().unwrap() {
                    "stat" => serde_json::json!({"ok":true,"stat":stat}),
                    "output-stream" => {
                        let attempt = attempts.fetch_add(1, Ordering::Relaxed);
                        if fail_first && attempt == 0 {
                            // No completed negotiation: a transport failure
                            // must not disable a potentially supported stream.
                            continue;
                        }
                        serde_json::json!({"ok":false,"error":"unknown variant `output-stream`"})
                    }
                    op => panic!("unexpected operation {op}"),
                };
                serde_json::to_writer(&mut stream, &response).unwrap();
                stream.write_all(b"\n").unwrap();
            }
        })
    };
    let holder = OldHolder {
        stop,
        attempts,
        worker: Some(worker),
    };
    let logs = root.path().join("logs");
    let mut log = OutputLog::writer(&logs, "s_compat").unwrap();
    let (engine, _) =
        ManifestEngine::load_dir(&diri_engine::detect::bundled_manifest_dir()).unwrap();
    let session = Session::adopt(
        SessionSpec {
            id: "s_compat".into(),
            pty: PtySpec::new(vec!["/bin/cat".into()], "/tmp").size(80, 24),
            manifest_id: "shell".into(),
            authority: Authority::ProcessOnly,
            logs_dir: logs,
            holder: Some(config.clone()),
            remote: None,
            defer_launch: false,
        },
        &config,
        &stat,
        Arc::new(engine),
    )
    .unwrap();
    wait_for(|| holder.attempts.load(Ordering::Relaxed) > 0);
    for index in 0..10 {
        let marker = format!("compat-output-{index}");
        log.append(format!("{marker}\r\n").as_bytes()).unwrap();
        wait_for(|| {
            session
                .screen_lines()
                .iter()
                .any(|line| line.contains(&marker))
        });
    }
    std::thread::sleep(Duration::from_millis(150));
    drop(session);
    let attempts = holder.attempts.load(Ordering::Relaxed);
    eprintln!("old-holder output negotiations across ten log updates: {attempts}");
    assert_eq!(
        attempts,
        if fail_first { 2 } else { 1 },
        "unsupported output streaming must stay on log transport"
    );
}

fn read_request(stream: &mut std::os::unix::net::UnixStream) -> serde_json::Value {
    // Keep accept nonblocking for shutdown, but wait for the client's request
    // on the accepted socket. macOS inherits the listener's nonblocking flag.
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

#[test]
fn fake_holder_waits_for_request_after_nonblocking_accept() {
    use std::os::unix::net::UnixStream;
    let (mut server, mut client) = UnixStream::pair().unwrap();
    // macOS can inherit this flag from the nonblocking listener. The client
    // need not have written its request when accept returns.
    server.set_nonblocking(true).unwrap();
    let writer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(30));
        let _ = client.write_all(b"{\"op\":\"stat\"}\n");
    });
    let request = read_request(&mut server);
    writer.join().unwrap();
    assert_eq!(request["op"], "stat");
}
