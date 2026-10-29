//! Opt-in (macOS, real launchd): a live Holder manager that is not a launchd
//! job stops taking new sessions once launchd works, without losing its own.
//!
//! A manager the Engine once spawned directly (launchd failed that day, or an
//! older diri) outlived every update and kept starting Agents for weeks.
//! After its GUI session ended those Agents could not open a browser or use
//! the pasteboard. Here such a manager hosts one session; the next launch
//! must go to a fresh launchd manager while the old session keeps running.
//!
//! ```sh
//! cargo test -p diri-engine --test launchd_manager_retire -- --ignored
//! ```
//!
//! Both sessions are ended at the end and both managers idle out after one
//! second; the launchd jobs are swept like any finished manager job.
#![cfg(target_os = "macos")]

use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use diri_engine::holder::agent_launcher::AGENT_LAUNCHER_ENV;
use diri_engine::holder::protocol::DEFAULT_DISK_CAPACITY;
use diri_engine::holder::{
    HolderClient, HolderLaunchSpec, HolderLauncher, HolderManagerClient, HolderManagerPaths,
    HolderPaths,
};

const PREFIX: &str = "com.dirijor.diri.holders.";

fn spec(root: &Path, session: &str) -> (HolderPaths, HolderLaunchSpec) {
    let paths = HolderPaths::new(root, session);
    let spec = HolderLaunchSpec {
        session_id: paths.session_id.clone(),
        socket_path: paths.socket().to_string_lossy().into_owned(),
        pid_file_path: paths.pid_file().to_string_lossy().into_owned(),
        log_file_path: root
            .join("logs")
            .join(format!("{session}.bin"))
            .to_string_lossy()
            .into_owned(),
        argv: vec!["/bin/cat".into()],
        cwd: "/tmp".into(),
        environment: HashMap::from([("PATH".to_string(), "/usr/bin:/bin".to_string())]),
        cols: 80,
        rows: 24,
        disk_capacity: DEFAULT_DISK_CAPACITY,
    };
    (paths, spec)
}

fn wait_alive(client: &HolderClient) -> bool {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !client.is_alive() {
        if Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    true
}

#[test]
#[ignore = "real launchd: creates a transient gui/<uid> job"]
fn a_direct_manager_is_retired_and_keeps_its_sessions() {
    let root = std::env::temp_dir().join(format!("diri-lmr-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("logs")).expect("holders dir");
    let holder = Path::new(env!("CARGO_BIN_EXE_diri-holder"));
    // SAFETY: set before any launch; this file holds one test.
    unsafe { std::env::set_var("DIRI_HOLDER_IDLE_SECONDS", "1") };

    // The stale manager: spawned directly, as the fallback does.
    let manager_paths = HolderManagerPaths::new(&root);
    let mut direct = Command::new(holder)
        .arg("--manager")
        .arg(&manager_paths.directory)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("direct manager");
    let manager = HolderManagerClient::new(manager_paths.socket());
    let deadline = Instant::now() + Duration::from_secs(5);
    while !manager.is_alive() {
        assert!(Instant::now() < deadline, "direct manager never answered");
        std::thread::sleep(Duration::from_millis(20));
    }
    let (old_paths, old_spec) = spec(&root, "s_lmr_old");
    let old_pid = manager.launch(&old_spec).expect("old session");
    assert_eq!(old_pid, direct.id() as i32);
    let old_session = HolderClient::new(old_paths.socket());
    assert!(wait_alive(&old_session));

    // Launchd works: any executable turns the launchd path on. The Agent
    // launch through it falls back to a direct spawn.
    // SAFETY: as above.
    unsafe { std::env::set_var(AGENT_LAUNCHER_ENV, "/usr/bin/true") };
    let (new_paths, new_spec) = spec(&root, "s_lmr_new");
    let new_pid = HolderLauncher::launch(holder, &new_paths, &new_spec).expect("new session");
    assert_ne!(new_pid, old_pid, "the direct manager took the new session");
    assert_eq!(
        diri_pty::detached::is_job_process(PREFIX, new_pid as u32),
        Some(true),
        "the successor is not a launchd job"
    );

    // The old session lives on in the retired manager.
    assert!(
        old_session.is_alive(),
        "retiring the manager ended its session"
    );
    let new_session = HolderClient::new(new_paths.socket());
    assert!(wait_alive(&new_session));

    for session in [&old_session, &new_session] {
        let _ = session.kill_tree();
    }
    // The retired manager idles out without removing its successor's endpoint.
    let deadline = Instant::now() + Duration::from_secs(20);
    while direct.try_wait().expect("wait").is_none() {
        assert!(Instant::now() < deadline, "retired manager never exited");
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = std::fs::remove_dir_all(&root);
}
