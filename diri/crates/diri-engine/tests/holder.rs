//! End-to-end holder tests: a real child on a real PTY, held by a real
//! holder, driven only through the socket protocol — the way the daemon will
//! drive it.

#![cfg(unix)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use diri_engine::OutputLog;
use diri_engine::holder::protocol::DEFAULT_DISK_CAPACITY;
use diri_engine::holder::{
    HolderClient, HolderExitMarker, HolderLaunchSpec, HolderLauncher, HolderManagerClient,
    HolderManagerPaths, HolderManagerServer, HolderPaths, HolderServer,
};

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
        cols: 80,
        rows: 24,
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

/// The log's whole payload from offset 0, via the same reader the daemon uses.
fn log_bytes(logs: &Path, session_id: &str) -> Vec<u8> {
    let mut log = OutputLog::reader(logs, session_id).expect("open log");
    log.refresh_from_disk();
    let tail = log.tail_offset();
    log.read(0, tail as usize).1
}

/// Short-path holder directories, or the socket path exceeds sun_path.
fn holders_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("diri-hold-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create holders dir");
    dir
}

#[test]
fn a_holder_owns_a_session_end_to_end() {
    let root = holders_dir("e2e");
    let logs = root.join("logs");
    let paths = HolderPaths::new(&root, "s_e2e");
    let launch = spec(&paths, &logs, &["/bin/cat"]);

    let server_spec = launch.clone();
    let server = std::thread::spawn(move || HolderServer::run(server_spec));

    let client = HolderClient::new(paths.socket());
    wait_until("holder ready", Duration::from_secs(5), || client.is_alive());

    let stat = client.stat().expect("stat");
    assert!(stat.alive);
    assert!(stat.child_pid > 1);
    assert_eq!(
        stat.epoch_offset,
        Some(0),
        "a fresh log starts this incarnation at offset zero"
    );
    assert_eq!((stat.cols, stat.rows), (Some(80), Some(24)));
    assert!(
        paths.pid_file().exists(),
        "the pid file names the serving process"
    );

    // cat echoes: written bytes come back through the PTY into the log.
    client.write(b"hello holder\n").expect("write");
    wait_until("echo in log", Duration::from_secs(5), || {
        log_bytes(&logs, "s_e2e")
            .windows(12)
            .any(|window| window == b"hello holder")
    });

    client.resize(132, 43).expect("resize");
    let resized = client.stat().expect("stat after resize");
    assert_eq!((resized.cols, resized.rows), (Some(132), Some(43)));

    // The tree is visible and killable through the protocol alone.
    let tree = client.signal(0).err(); // 0 is invalid, must be rejected
    assert!(tree.is_some(), "signal 0 must be rejected");
    client.kill_tree().expect("kill-tree");

    server
        .join()
        .expect("join")
        .expect("the holder run ends cleanly after its child dies");

    // The exit marker records the SIGTERM/SIGKILL death, in-band.
    let mut buffer = log_bytes(&logs, "s_e2e");
    let (_, exit) = HolderExitMarker::drain(&mut buffer);
    let exit = exit.expect("an exit marker is in the log");
    assert!(
        exit.signal.is_some(),
        "kill-tree death is signalled: {exit:?}"
    );

    assert!(!paths.socket().exists(), "control files are removed");
    assert!(!paths.pid_file().exists());
}

#[test]
fn a_clean_exit_writes_the_code_into_the_marker() {
    let root = holders_dir("exit");
    let logs = root.join("logs");
    let paths = HolderPaths::new(&root, "s_exit");
    let launch = spec(&paths, &logs, &["/bin/sh", "-c", "exit 3"]);

    HolderServer::run(launch).expect("run to completion");

    let mut buffer = log_bytes(&logs, "s_exit");
    let (_, exit) = HolderExitMarker::drain(&mut buffer);
    assert_eq!(exit.expect("marker").code, Some(3));
}

#[test]
fn a_second_holder_for_the_same_session_refuses_to_double_run() {
    let root = holders_dir("dbl");
    let logs = root.join("logs");
    let paths = HolderPaths::new(&root, "s_dbl");
    let launch = spec(&paths, &logs, &["/bin/cat"]);

    let first_spec = launch.clone();
    let first = std::thread::spawn(move || HolderServer::run(first_spec));
    let client = HolderClient::new(paths.socket());
    wait_until("holder ready", Duration::from_secs(5), || client.is_alive());

    let error = HolderServer::run(launch).expect_err("second run must refuse");
    assert!(
        error.to_string().contains("refusing to double-run"),
        "{error}"
    );
    assert!(
        client.is_alive(),
        "the live holder is undisturbed by the refusal"
    );

    client.kill_tree().expect("kill");
    first.join().expect("join").expect("first run ends cleanly");
}

#[test]
fn a_relaunch_under_the_same_session_id_starts_a_new_epoch() {
    let root = holders_dir("epoch");
    let logs = root.join("logs");
    let paths = HolderPaths::new(&root, "s_epoch");

    HolderServer::run(spec(&paths, &logs, &["/bin/sh", "-c", "echo one; exit 0"]))
        .expect("first incarnation");
    let first_tail = {
        let mut log = OutputLog::reader(&logs, "s_epoch").expect("log");
        log.refresh_from_disk();
        log.tail_offset()
    };
    assert!(first_tail > 0);

    let relaunch = spec(&paths, &logs, &["/bin/cat"]);
    let server = std::thread::spawn(move || HolderServer::run(relaunch));
    let client = HolderClient::new(paths.socket());
    wait_until("holder ready", Duration::from_secs(5), || client.is_alive());

    let stat = client.stat().expect("stat");
    assert_eq!(
        stat.epoch_offset,
        Some(first_tail),
        "bytes below the epoch — including the first incarnation's exit \
         marker — belong to the previous child"
    );

    client.kill_tree().expect("kill");
    server.join().expect("join").expect("clean end");
}

#[test]
fn the_manager_hosts_launches_and_idles_out() {
    let root = holders_dir("mgr");
    let logs = root.join("logs");
    let manager_paths = HolderManagerPaths::new(&root);

    let idle = Duration::from_millis(600);
    let run_root = root.clone();
    let manager_thread =
        std::thread::spawn(move || HolderManagerServer::new(&run_root, idle).run());

    let manager = HolderManagerClient::new(manager_paths.socket());
    wait_until("manager ready", Duration::from_secs(5), || {
        manager.is_alive()
    });

    let paths = HolderPaths::new(&root, "s_mgr");
    let launch = spec(&paths, &logs, &["/bin/cat"]);
    let pid = manager.launch(&launch).expect("launch");
    assert_eq!(pid, std::process::id() as i32, "in-process manager pid");

    let client = HolderClient::new(paths.socket());
    wait_until("session ready", Duration::from_secs(5), || {
        client.is_alive()
    });

    // Launching the same spec again adopts the live holder, no second child.
    let first_child = client.stat().expect("stat").child_pid;
    manager.launch(&launch).expect("re-launch");
    assert_eq!(client.stat().expect("stat").child_pid, first_child);

    // A spec whose control files point elsewhere is rejected.
    let mut foreign = launch.clone();
    foreign.socket_path = "/tmp/elsewhere.sock".into();
    assert!(manager.launch(&foreign).is_err());

    client.kill_tree().expect("kill");
    wait_until("session gone", Duration::from_secs(5), || {
        !client.is_alive()
    });

    // With no sessions hosted, the manager idles out and cleans up after
    // itself.
    manager_thread
        .join()
        .expect("join")
        .expect("idle exit is a clean exit");
    assert!(!manager_paths.socket().exists());
    assert!(!manager_paths.pid_file().exists());
}

#[test]
fn the_launcher_bootstraps_a_real_manager_process() {
    let root = holders_dir("bin");
    let logs = root.join("logs");
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_diri-holder"));

    // Keep the detached manager from outliving the test on failure paths.
    // SAFETY: setenv before any launch; tests in this file are process-wide.
    unsafe { std::env::set_var("DIRI_HOLDER_IDLE_SECONDS", "1") };

    let paths = HolderPaths::new(&root, "s_bin");
    let launch = spec(&paths, &logs, &["/bin/cat"]);
    let manager_pid =
        HolderLauncher::launch(&binary, &paths, &launch).expect("launcher bootstraps");
    assert!(manager_pid > 1);

    let client = HolderClient::new(paths.socket());
    wait_until("session ready", Duration::from_secs(5), || {
        client.is_alive()
    });

    // A second launch for the same session adopts rather than duplicates.
    let adopted = HolderLauncher::launch(&binary, &paths, &launch).expect("adopt");
    assert_eq!(adopted, manager_pid);

    // The holder survives this "daemon": nothing here holds its PTY. Drive it
    // from a brand-new client, as a restarted daemon would.
    let fresh = HolderClient::new(paths.socket());
    fresh.write(b"survived\n").expect("write after adopt");
    wait_until("echo in log", Duration::from_secs(5), || {
        log_bytes(&logs, "s_bin")
            .windows(8)
            .any(|window| window == b"survived")
    });

    fresh.kill_tree().expect("kill");
    wait_until("session gone", Duration::from_secs(5), || !fresh.is_alive());

    // The detached manager idles out shortly after its last session ends.
    // Watch passively — a ping would re-arm the idle timer (by design: a
    // live daemon's pings keep the manager warm).
    wait_until("manager idle exit", Duration::from_secs(10), || {
        // SAFETY: kill with signal 0 only checks existence.
        unsafe { libc::kill(manager_pid, 0) != 0 }
    });
    assert!(
        !HolderManagerPaths::new(&root).socket().exists(),
        "an idle exit removes the manager endpoint"
    );
}
