//! End-to-end holder tests: a real child on a real PTY, held by a real
//! holder, driven only through the socket protocol — the way the daemon will
//! drive it.

#![cfg(unix)]

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use base64::Engine as _;
use diri_engine::OutputLog;
use diri_engine::holder::protocol::{
    DEFAULT_DISK_CAPACITY, HolderOperation, HolderRequest, HolderResponse,
};
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
            ("HOME".to_string(), "/tmp".to_string()),
            ("PS1".to_string(), "$ ".to_string()),
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

fn legacy_write(path: &Path, bytes: &[u8]) {
    let mut request = HolderRequest::op(HolderOperation::Write);
    request.data = Some(base64::engine::general_purpose::STANDARD.encode(bytes));
    let mut stream = UnixStream::connect(path).expect("legacy connect");
    serde_json::to_writer(&mut stream, &request).expect("legacy encode");
    stream.write_all(b"\n").expect("legacy request");
    let mut response = Vec::new();
    let mut byte = [0_u8; 1];
    while stream.read_exact(&mut byte).is_ok() && byte[0] != b'\n' {
        response.push(byte[0]);
    }
    let response: HolderResponse = serde_json::from_slice(&response).expect("legacy response");
    assert!(response.ok, "legacy input rejected: {:?}", response.error);
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
    let identity = stat
        .verified_child_identity()
        .expect("new Holder reports exact owned-child birth");
    assert_eq!(identity.pid(), stat.child_pid as u32);
    assert_eq!(
        diri_pty::process_identity::observe(identity.pid()).unwrap(),
        identity
    );
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
    assert_eq!(resized.verified_child_identity(), Some(identity));
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
fn a_holder_records_what_it_forked_before_a_fast_child_can_vanish() {
    let root = holders_dir("child-record");
    let logs = root.join("logs");
    let paths = HolderPaths::new(&root, "s_child_record");
    // Exits within a millisecond: the socket and pid file are gone before a
    // client could ever ask, which is exactly when the record must exist.
    let launch = spec(&paths, &logs, &["/bin/sh", "-c", "exit 3"]);
    let server = std::thread::spawn(move || HolderServer::run(launch));
    server
        .join()
        .expect("join")
        .expect("the holder run ends cleanly after its child exits");
    assert!(!paths.socket().exists() && !paths.pid_file().exists());

    let record = diri_engine::holder::protocol::HolderChildRecord::read(&paths.child_record())
        .expect("the child record outlives the Holder");
    assert!(record.child_pid > 1);
    assert_eq!(
        record.epoch_offset, 0,
        "a fresh log starts this incarnation at zero"
    );
    let identity = record
        .child_identity
        .expect("the Holder recorded the child's birth identity at spawn");
    assert_eq!(identity.pid(), record.child_pid as u32);
    let stat = record.as_stat();
    assert!(!stat.alive, "a record never claims the child is alive");
    assert_eq!(stat.verified_child_identity(), Some(identity));
    assert_eq!(stat.epoch_offset, Some(0));
}

#[test]
#[ignore = "release-only local UDS input latency benchmark"]
fn holder_input_latency_is_reported() {
    let root = holders_dir("lat");
    let logs = root.join("logs");
    let paths = HolderPaths::new(&root, "s_latency");
    let launch = spec(&paths, &logs, &["/bin/cat"]);
    let server = std::thread::spawn(move || HolderServer::run(launch));
    let client = HolderClient::new(paths.socket());
    wait_until("holder ready", Duration::from_secs(5), || client.is_alive());

    // Measure the exact pre-upgrade request shape on the same Holder and
    // machine, then compare the persistent stream without scheduler or build
    // differences muddying the result.
    for _ in 0..20 {
        legacy_write(&paths.socket(), b"x");
    }
    let mut legacy = Vec::with_capacity(500);
    for _ in 0..500 {
        let started = Instant::now();
        legacy_write(&paths.socket(), b"x");
        legacy.push(started.elapsed());
    }
    legacy.sort_unstable();

    for _ in 0..20 {
        client.write(b"x").expect("warm stream input");
    }
    let mut samples = Vec::with_capacity(500);
    for _ in 0..500 {
        let started = Instant::now();
        client.write(b"x").expect("input");
        samples.push(started.elapsed());
    }
    samples.sort_unstable();
    let legacy_p50 = legacy[legacy.len() / 2];
    let legacy_p95 = legacy[legacy.len() * 95 / 100];
    let p50 = samples[samples.len() / 2];
    let p95 = samples[samples.len() * 95 / 100];
    eprintln!(
        "local holder input: legacy p50 {}us/p95 {}us; stream p50 {}us/p95 {}us",
        legacy_p50.as_micros(),
        legacy_p95.as_micros(),
        p50.as_micros(),
        p95.as_micros()
    );
    assert!(
        p95 < legacy_p95,
        "persistent input p95 {p95:?} did not beat legacy {legacy_p95:?}"
    );
    assert!(
        p95 <= Duration::from_millis(1),
        "persistent local input p95 {p95:?} exceeded the 1ms gate"
    );

    client.kill_tree().expect("kill");
    server.join().expect("join").expect("clean end");
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
fn stat_reports_a_foreground_job_other_than_the_shell() {
    let root = holders_dir("fgjob");
    let logs = root.join("logs");
    let paths = HolderPaths::new(&root, "s_fgjob");
    let launch = spec(&paths, &logs, &["/bin/bash", "--norc", "--noprofile", "-i"]);
    let server = std::thread::spawn(move || HolderServer::run(launch));
    let client = HolderClient::new(paths.socket());
    wait_until("holder ready", Duration::from_secs(5), || client.is_alive());

    let child = client.stat().expect("stat").child_pid;
    wait_until("shell claimed tty", Duration::from_secs(2), || {
        client
            .stat()
            .ok()
            .is_some_and(|stat| stat.foreground_pid == Some(child))
    });
    client.write(b"sleep 8\n").expect("write sleep");
    wait_until("foreground job", Duration::from_secs(3), || {
        client.stat().ok().is_some_and(|stat| {
            stat.foreground_pid
                .is_some_and(|pgid| pgid > 0 && pgid != child)
        })
    });

    client.kill_tree().expect("kill");
    server.join().expect("join").expect("clean end");
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
    let first_stat = client.stat().expect("stat");
    let first_child = first_stat.child_pid;
    let first_identity = first_stat
        .verified_child_identity()
        .expect("owned child birth");
    manager.launch(&launch).expect("re-launch");
    let adopted_stat = client.stat().expect("stat");
    assert_eq!(adopted_stat.child_pid, first_child);
    assert_eq!(adopted_stat.verified_child_identity(), Some(first_identity));

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

/// Gone, or a zombie waiting for init: either way nothing left running.
fn process_dead(pid: i32) -> bool {
    let output = std::process::Command::new("/bin/ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .expect("ps");
    let state = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    state.is_empty() || state.starts_with('Z')
}

/// The manager dying takes a hibernated session's tree with it. Before the
/// group guard, a crash, `kill -9` or reinstall of the manager hung the PTY up;
/// the stopped leader died of the hangup, and so did any stopped member with
/// default signal handling, but a member that handles SIGHUP — Codex's node
/// wrapper, Claude Code — stayed stopped under launchd forever, and so did
/// any stopped helper in a session of its own.
#[test]
fn a_dead_manager_leaves_no_hibernated_agent_behind() {
    let root = holders_dir("guard");
    let logs = root.join("logs");
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_diri-holder"));
    let pids = root.join("pids");
    let wrapper = root.join("codex.sh");
    std::fs::write(
        &wrapper,
        format!(
            "trap 'kill -TERM $child 2>/dev/null' TERM HUP INT\n\
             perl -e 'use POSIX; POSIX::setsid(); sleep 1000' &\n\
             helper=$!\n\
             sleep 1000 & child=$!\n\
             echo $$ $child $helper > {pids}.tmp && mv {pids}.tmp {pids}\n\
             wait $child; wait $child\n",
            pids = pids.display()
        ),
    )
    .expect("write wrapper");

    let paths = HolderPaths::new(&root, "s_guard");
    let script = format!("/bin/sh {}; true", wrapper.display());
    let launch = spec(&paths, &logs, &["/bin/sh", "-c", &script]);
    let manager_pid = HolderLauncher::launch(&binary, &paths, &launch).expect("launch");
    let client = HolderClient::new(paths.socket());
    wait_until("session ready", Duration::from_secs(5), || {
        client.is_alive()
    });
    let mut agent: Vec<i32> = Vec::new();
    wait_until("agent tree", Duration::from_secs(5), || {
        agent = std::fs::read_to_string(&pids)
            .ok()
            .filter(|text| text.ends_with('\n'))
            .map(|text| {
                text.split_whitespace()
                    .filter_map(|w| w.parse().ok())
                    .collect()
            })
            .unwrap_or_default();
        agent.len() == 3
    });
    // The helper leaves the group, as Chrome DevTools MCP's watchdog does.
    wait_until("helper in its own session", Duration::from_secs(5), || {
        // SAFETY: read-only getpgid on a pid this test started.
        unsafe { libc::getpgid(agent[2]) == agent[2] }
    });

    let frozen = client.signal(libc::SIGSTOP).expect("hibernate");
    assert!(agent.iter().all(|pid| frozen.iter().any(|s| s.pid == *pid)));

    // SAFETY: the manager this test launched.
    unsafe { libc::kill(manager_pid, libc::SIGKILL) };

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !agent.iter().all(|&pid| process_dead(pid)) {
        std::thread::sleep(Duration::from_millis(20));
    }
    let survivors: Vec<i32> = agent
        .iter()
        .copied()
        .filter(|&pid| !process_dead(pid))
        .collect();
    for pid in &survivors {
        // SAFETY: cleanup of this test's own leaked processes.
        unsafe {
            libc::kill(*pid, libc::SIGKILL);
            libc::kill(*pid, libc::SIGCONT);
        }
    }
    assert!(
        survivors.is_empty(),
        "outlived the manager: {survivors:?} of {agent:?}"
    );
}

#[test]
fn a_paste_larger_than_one_input_frame_arrives_whole() {
    let root = holders_dir("paste");
    let logs = root.join("logs");
    let paths = HolderPaths::new(&root, "s_paste");
    let size = diri_engine::holder::protocol::HOLDER_STREAM_MAX_PAYLOAD * 3 / 2;
    let script = format!("stty -echo -icanon; head -c {size} | wc -c; exec cat");
    let launch = spec(&paths, &logs, &["/bin/sh", "-c", &script]);
    let server = std::thread::spawn(move || HolderServer::run(launch));
    let client = HolderClient::new(paths.socket());
    wait_until("holder ready", Duration::from_secs(5), || client.is_alive());
    // Let stty run before the paste lands.
    std::thread::sleep(Duration::from_millis(300));

    // A 1.5 MiB paste used to be rejected whole by the Holder's frame bound.
    let paste = b"0123456789abcdef".repeat(size / 16);
    client.write(&paste).expect("a large paste is delivered");
    let expected = size.to_string();
    wait_until("the whole paste read", Duration::from_secs(20), || {
        String::from_utf8_lossy(&log_bytes(&logs, "s_paste")).contains(&expected)
    });

    client.kill_tree().expect("kill");
    let _ = server.join();
    let _ = std::fs::remove_dir_all(&root);
}

/// Opt-in upgrade rehearsal: a manager from the previous release (protocol
/// v1) still hosting a session when this build's Engine launches a new one.
/// The new session must get a manager of this version beside the old one,
/// not be launched through it; the old session stays where it is, and the
/// old manager retires once its own sessions end.
///
/// ```sh
/// git worktree add --detach /tmp/diri-prev origin/main
/// (cd /tmp/diri-prev/diri && cargo build -p diri-engine --bin diri-holder)
/// DIRI_PREVIOUS_HOLDER=<that target>/debug/diri-holder \
///   cargo test -p diri-engine --test holder an_upgrade -- --ignored
/// ```
#[test]
#[ignore = "needs a previous release's diri-holder; set DIRI_PREVIOUS_HOLDER"]
fn an_upgrade_starts_a_new_manager_beside_a_running_older_one() {
    use diri_engine::holder::protocol::HolderManagerRequest;
    let previous =
        PathBuf::from(std::env::var_os("DIRI_PREVIOUS_HOLDER").expect("DIRI_PREVIOUS_HOLDER"));
    let root = holders_dir("upgrade");
    let logs = root.join("logs");
    // SAFETY: set before any launch; the managers inherit it.
    unsafe { std::env::set_var("DIRI_HOLDER_IDLE_SECONDS", "1") };

    // The existing user's manager and session, from before the update.
    let mut old_manager = std::process::Command::new(&previous)
        .arg("--manager")
        .arg(&root)
        .spawn()
        .expect("previous manager");
    let old_socket = HolderPaths::new(&root, "unused")
        .directory
        .join("manager-v1.sock");
    wait_until("previous manager", Duration::from_secs(5), || {
        UnixStream::connect(&old_socket).is_ok()
    });
    let old_paths = HolderPaths::new(&root, "s_before");
    let mut launch = HolderManagerRequest::launch(spec(&old_paths, &logs, &["/bin/cat"]));
    launch.version = 1;
    {
        let mut stream = UnixStream::connect(&old_socket).expect("connect");
        serde_json::to_writer(&mut stream, &launch).expect("encode");
        stream.write_all(b"\n").expect("send");
        let mut byte = [0_u8; 1];
        while stream.read_exact(&mut byte).is_ok() && byte[0] != b'\n' {}
    }
    let before = HolderClient::new(old_paths.socket());
    wait_until("session from before", Duration::from_secs(5), || {
        before.is_alive()
    });

    // After the update: this build launches a new session.
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_diri-holder"));
    let new_paths = HolderPaths::new(&root, "s_after");
    let new_manager =
        HolderLauncher::launch(&binary, &new_paths, &spec(&new_paths, &logs, &["/bin/cat"]))
            .expect("launch after the update");
    assert_ne!(
        new_manager as u32,
        old_manager.id(),
        "not through the old manager"
    );
    assert!(
        HolderManagerPaths::new(&root)
            .socket()
            .ends_with("manager-v2.sock")
    );
    let after = HolderClient::new(new_paths.socket());
    wait_until("session after", Duration::from_secs(5), || after.is_alive());
    assert!(before.is_alive(), "the session from before keeps running");

    // The old session still works, then ends; its manager retires on its own.
    before.write(b"still here\n").expect("write");
    wait_until("echo", Duration::from_secs(5), || {
        String::from_utf8_lossy(&log_bytes(&logs, "s_before")).contains("still here")
    });
    before.kill_tree().expect("end old session");
    wait_until("previous manager retires", Duration::from_secs(10), || {
        old_manager.try_wait().unwrap().is_some()
    });
    assert!(after.is_alive(), "the new session is untouched");
    after.kill_tree().expect("end new session");
}

/// One raw manager request carrying `engine_pid` as its sender, the way an
/// Engine with that pid would send it.
fn manager_request(socket: &Path, request: &diri_engine::holder::protocol::HolderManagerRequest) {
    let mut stream = UnixStream::connect(socket).expect("manager connect");
    serde_json::to_writer(&mut stream, request).expect("encode");
    stream.write_all(b"\n").expect("send");
    let mut response = Vec::new();
    let mut byte = [0_u8; 1];
    while stream.read_exact(&mut byte).is_ok() && byte[0] != b'\n' {
        response.push(byte[0]);
    }
}

/// Development builds: a manager whose Engine is gone, with no other Engine
/// checking in, ends its sessions instead of hosting them until reboot.
#[test]
fn a_development_manager_ends_sessions_no_engine_returns_for() {
    use diri_engine::holder::protocol::HolderManagerRequest;
    let root = holders_dir("abandon");
    let logs = root.join("logs");
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_diri-holder"));
    let manager_paths = HolderManagerPaths::new(&root);
    // Two stand-in Engines: only their pids and lifetimes matter.
    let mut first = std::process::Command::new("/bin/sleep")
        .arg("60")
        .spawn()
        .unwrap();
    let mut second = std::process::Command::new("/bin/sleep")
        .arg("60")
        .spawn()
        .unwrap();
    let mut manager = std::process::Command::new(&binary)
        .arg("--manager")
        .arg(&root)
        .arg(diri_engine::holder::manager::ENGINE_PID_FLAG)
        .arg(first.id().to_string())
        .env("DIRI_HOLDER_ABANDON_SECONDS", "1")
        .env("DIRI_HOLDER_IDLE_SECONDS", "1")
        .spawn()
        .expect("manager");
    let manager_client = HolderManagerClient::new(manager_paths.socket());
    wait_until("manager", Duration::from_secs(5), || {
        manager_client.is_alive()
    });

    let paths = HolderPaths::new(&root, "s_abandon");
    let mut launch = HolderManagerRequest::launch(spec(&paths, &logs, &["/bin/cat"]));
    launch.engine_pid = Some(first.id() as i32);
    manager_request(&manager_paths.socket(), &launch);
    let session = HolderClient::new(paths.socket());
    wait_until("session", Duration::from_secs(5), || session.is_alive());

    // Another Engine checks in, then the first goes: the session stays.
    let mut ping = HolderManagerRequest::ping();
    ping.engine_pid = Some(second.id() as i32);
    manager_request(&manager_paths.socket(), &ping);
    first.kill().unwrap();
    first.wait().unwrap();
    std::thread::sleep(Duration::from_secs(2));
    assert!(session.is_alive(), "a returning Engine keeps its sessions");

    // The last Engine goes and none returns: the session is ended, and the
    // manager, now idle, retires.
    second.kill().unwrap();
    second.wait().unwrap();
    wait_until("abandoned session ended", Duration::from_secs(10), || {
        !session.is_alive()
    });
    let mut buffer = log_bytes(&logs, "s_abandon");
    assert!(
        HolderExitMarker::drain(&mut buffer).1.is_some(),
        "it ended cleanly"
    );
    wait_until("manager retired", Duration::from_secs(10), || {
        manager.try_wait().unwrap().is_some()
    });
}

/// Child half of the pixel-size tests: run as a held process, it waits for a
/// line and prints the terminal size its PTY reports. A no-op when the test
/// harness runs it directly.
#[test]
fn winsize_probe() {
    if std::env::var_os("DIRI_WINSIZE_PROBE").is_none() {
        return;
    }
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).expect("line");
    // SAFETY: zeroed POD out-parameter for TIOCGWINSZ on stdin.
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: fd 0 is the PTY and `size` is a valid out-pointer.
    assert_eq!(unsafe { libc::ioctl(0, libc::TIOCGWINSZ, &mut size) }, 0);
    println!(
        "winsize={}x{}/{}x{}",
        size.ws_col, size.ws_row, size.ws_xpixel, size.ws_ypixel
    );
}

#[test]
fn a_resize_carries_the_text_area_in_pixels() {
    let root = holders_dir("pixels");
    let logs = root.join("logs");
    let paths = HolderPaths::new(&root, "s_pixels");
    let probe = std::env::current_exe().expect("test binary");
    let mut launch = spec(
        &paths,
        &logs,
        &[
            probe.to_str().unwrap(),
            "--exact",
            "winsize_probe",
            "--nocapture",
            "--test-threads=1",
        ],
    );
    launch
        .environment
        .insert("DIRI_WINSIZE_PROBE".into(), "1".into());
    let server = std::thread::spawn(move || HolderServer::run(launch));
    let client = HolderClient::new(paths.socket());
    wait_until("holder ready", Duration::from_secs(5), || client.is_alive());

    client
        .resize_with_pixels(100, 30, (1600, 1020))
        .expect("resize");
    client.write(b"\n").expect("line");
    wait_until("the child's size", Duration::from_secs(10), || {
        String::from_utf8_lossy(&log_bytes(&logs, "s_pixels")).contains("winsize=100x30/1600x1020")
    });

    let _ = client.kill_tree();
    let _ = server.join();
    let _ = std::fs::remove_dir_all(&root);
}

/// A Holder from before pixel resizes refuses input stream version 2 and
/// knows only the four-byte resize. The client asks again for version 1 and
/// sends that.
#[test]
fn a_holder_without_pixel_resizes_still_resizes() {
    use std::io::BufRead;
    use std::os::unix::net::UnixListener;

    let root = holders_dir("pixels-old");
    let socket = root.join("old.sock");
    let listener = UnixListener::bind(&socket).expect("bind");
    let old = std::thread::spawn(move || {
        let mut frames = Vec::new();
        for connection in listener.incoming().take(2) {
            let mut connection = connection.expect("connection");
            let mut reader = std::io::BufReader::new(connection.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).expect("request");
            let request: HolderRequest = serde_json::from_str(&line).expect("request json");
            assert_eq!(request.op, HolderOperation::Stream);
            if request.stream_version != Some(1) {
                let refusal = HolderResponse::failure("unsupported Holder input stream version");
                let mut json = serde_json::to_vec(&refusal).unwrap();
                json.push(b'\n');
                connection.write_all(&json).unwrap();
                continue;
            }
            let mut json = serde_json::to_vec(&HolderResponse::stream(1)).unwrap();
            json.push(b'\n');
            connection.write_all(&json).unwrap();
            let mut header = [0u8; 5];
            reader.read_exact(&mut header).expect("frame");
            let mut payload =
                vec![0u8; u32::from_be_bytes(header[1..].try_into().unwrap()) as usize];
            reader.read_exact(&mut payload).expect("payload");
            frames.push((header[0], payload));
            connection.write_all(&[0]).unwrap();
        }
        frames
    });

    HolderClient::new(&socket)
        .resize_with_pixels(100, 30, (1600, 1020))
        .expect("resize through the old protocol");
    let frames = old.join().unwrap();
    assert_eq!(frames, vec![(2, vec![0, 100, 0, 30])]);
    let _ = std::fs::remove_dir_all(&root);
}
