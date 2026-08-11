//! Cross-engine holder interop: the Rust client, launcher, log reader, and
//! exit-marker parser against the REAL Swift `dirijord-holder` binary.
//!
//! This is the compatibility a live upgrade depends on: a Rust daemon must
//! adopt holders the Swift daemon spawned, byte for byte — same launch-spec
//! JSON, same socket protocol, same output-log format, same exit marker.
//!
//! Skips (loudly) when the Swift binary has not been built. Build it with:
//!
//! ```sh
//! cd ../.. && swift build --product dirijord-holder
//! ```

#![cfg(unix)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use diri_engine::OutputLog;
use diri_engine::holder::protocol::DEFAULT_DISK_CAPACITY;
use diri_engine::holder::{
    HolderClient, HolderExitMarker, HolderLaunchSpec, HolderLauncher, HolderPaths,
};

/// The Swift holder binary, if the outer package has been built.
fn swift_holder() -> Option<PathBuf> {
    let candidate =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.build/debug/dirijord-holder");
    match candidate.canonicalize() {
        Ok(path) => Some(path),
        Err(_) => {
            eprintln!(
                "SKIPPED: Swift dirijord-holder not built at {}; \
                 run `swift build --product dirijord-holder` in the outer repo",
                candidate.display()
            );
            None
        }
    }
}

fn holders_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("diri-interop-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create holders dir");
    dir
}

fn spec(paths: &HolderPaths, logs: &Path, argv: &[&str]) -> HolderLaunchSpec {
    HolderLaunchSpec {
        session_id: paths.session_id.clone(),
        socket_path: paths.socket().to_string_lossy().into_owned(),
        pid_file_path: paths.pid_file().to_string_lossy().into_owned(),
        log_file_path: logs
            .join(format!("{}.bin", paths.session_id))
            .to_string_lossy()
            .into_owned(),
        argv: argv.iter().map(|word| word.to_string()).collect(),
        cwd: "/tmp".into(),
        environment: HashMap::from([
            (
                "PATH".to_string(),
                std::env::var("PATH").unwrap_or_default(),
            ),
            ("TERM".to_string(), "xterm-256color".to_string()),
        ]),
        cols: 100,
        rows: 30,
        disk_capacity: DEFAULT_DISK_CAPACITY,
    }
}

fn wait_until(what: &str, timeout: Duration, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if check() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for {what}");
}

fn log_bytes(logs: &Path, session_id: &str) -> Vec<u8> {
    let mut log = OutputLog::reader(logs, session_id).expect("open log");
    log.refresh_from_disk();
    let tail = log.tail_offset();
    log.read(0, tail as usize).1
}

/// Rust daemon ⇄ Swift holder, direct `--spec` mode: the Rust-encoded launch
/// spec must parse in Swift, and everything the Swift holder produces — stat
/// responses, the output log, the exit marker — must parse in Rust.
#[test]
fn the_rust_client_drives_a_live_swift_holder() {
    let Some(binary) = swift_holder() else { return };
    let root = holders_dir("spec");
    let logs = root.join("logs");
    let paths = HolderPaths::new(&root, "s_interop");
    let launch = spec(&paths, &logs, &["/bin/cat"]);

    // The spec file crosses the encoder/decoder boundary: Rust writes it,
    // Swift reads it.
    let spec_path = root.join("s_interop.launch.json");
    std::fs::write(&spec_path, serde_json::to_vec(&launch).expect("encode")).expect("write spec");
    let mut holder = std::process::Command::new(&binary)
        .arg("--spec")
        .arg(&spec_path)
        .spawn()
        .expect("spawn swift holder");

    let client = HolderClient::new(paths.socket());
    wait_until("swift holder ready", Duration::from_secs(10), || {
        client.is_alive()
    });

    // Swift's stat, decoded by Rust — including the epoch field.
    let stat = client.stat().expect("stat");
    assert!(stat.alive);
    assert!(stat.child_pid > 1);
    assert_eq!(stat.epoch_offset, Some(0));
    assert_eq!((stat.cols, stat.rows), (Some(100), Some(30)));

    // Rust write → Swift holder → PTY echo → Swift-written log → Rust reader.
    client.write(b"across the engines\n").expect("write");
    wait_until("echo in swift-written log", Duration::from_secs(10), || {
        log_bytes(&logs, "s_interop")
            .windows(18)
            .any(|window| window == b"across the engines")
    });

    client.resize(90, 40).expect("resize");
    let resized = client.stat().expect("stat");
    assert_eq!((resized.cols, resized.rows), (Some(90), Some(40)));

    // The signal op returns Swift's process samples; they must decode.
    let tree = client.signal(libc::SIGCONT).expect("signal");
    assert!(
        tree.iter().any(|sample| sample.pid == stat.child_pid),
        "the child is in the signalled tree: {tree:?}"
    );

    client.kill_tree().expect("kill-tree");
    let status = holder.wait().expect("swift holder exits");
    assert!(status.success(), "swift holder run ends cleanly: {status}");

    // Swift's exit marker, parsed by the Rust drainer.
    let mut buffer = log_bytes(&logs, "s_interop");
    let (_, exit) = HolderExitMarker::drain(&mut buffer);
    let exit = exit.expect("swift wrote an exit marker rust can read");
    assert!(exit.signal.is_some(), "killed, so signaled: {exit:?}");

    assert!(
        !paths.socket().exists(),
        "swift cleaned up its control files"
    );
}

/// Rust launcher ⇄ Swift manager: the Rust daemon bootstraps the SWIFT
/// manager binary and asks it to host a session — the exact path a Rust
/// daemon takes on a machine whose holder binary is still the Swift one.
#[test]
fn the_rust_launcher_bootstraps_a_swift_manager() {
    let Some(binary) = swift_holder() else { return };
    let root = holders_dir("mgr");
    let logs = root.join("logs");
    let paths = HolderPaths::new(&root, "s_swiftmgr");
    let launch = spec(&paths, &logs, &["/bin/cat"]);

    let manager_pid =
        HolderLauncher::launch(&binary, &paths, &launch).expect("bootstrap swift manager");
    assert!(manager_pid > 1);

    let client = HolderClient::new(paths.socket());
    wait_until("session ready", Duration::from_secs(10), || {
        client.is_alive()
    });

    // Re-launching adopts, across the language boundary.
    let adopted = HolderLauncher::launch(&binary, &paths, &launch).expect("adopt");
    assert_eq!(adopted, manager_pid);

    client.write(b"held by swift\n").expect("write");
    wait_until("echo in log", Duration::from_secs(10), || {
        log_bytes(&logs, "s_swiftmgr")
            .windows(13)
            .any(|window| window == b"held by swift")
    });

    client.kill_tree().expect("kill");
    wait_until("session gone", Duration::from_secs(10), || {
        !client.is_alive()
    });

    // Don't wait out the Swift manager's 30s idle window; it is detached and
    // no longer holds anything of ours.
    // SAFETY: killing the manager this test itself caused to start.
    unsafe { libc::kill(manager_pid, libc::SIGKILL) };
}
