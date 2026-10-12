//! Opt-in (macOS, real launchd): the Holder manager's launchd job is
//! submitted by the relay job, never by the launching Engine.
//!
//! loginwindow unloads every launchd job a process of diri.app's coalition
//! submitted when the app exits without Background Task Management approval;
//! 0.9.5 lost every session at each quit on such Macs because the Engine
//! submitted the manager's job itself. Here the launch goes through the relay:
//! the job the caller bootstrapped has exited and the manager runs under a
//! second label.
//!
//! ```sh
//! cargo test -p diri-engine --test launchd_manager_relay -- --ignored
//! ```
//!
//! The session is killed at the end and the manager idles out after one
//! second; launchd keeps both finished job records until the next sweep.
#![cfg(target_os = "macos")]

use std::collections::HashMap;
use std::process::Command;
use std::time::{Duration, Instant};

use diri_engine::holder::agent_launcher::AGENT_LAUNCHER_ENV;
use diri_engine::holder::protocol::DEFAULT_DISK_CAPACITY;
use diri_engine::holder::{HolderClient, HolderLaunchSpec, HolderLauncher, HolderPaths};

const PREFIX: &str = "com.dirijor.diri.holders.";

/// `launchctl list` rows with diri's manager prefix: (pid, last status, label).
fn manager_jobs() -> Vec<(String, String, String)> {
    let output = Command::new("/bin/launchctl")
        .arg("list")
        .output()
        .expect("launchctl list");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut columns = line.split('\t');
            let pid = columns.next()?.trim().to_string();
            let status = columns.next()?.trim().to_string();
            let label = columns.next()?.trim().to_string();
            label.starts_with(PREFIX).then_some((pid, status, label))
        })
        .collect()
}

#[test]
#[ignore = "real launchd: creates transient gui/<uid> jobs"]
fn the_manager_job_is_submitted_by_the_relay() {
    let root = std::env::temp_dir().join(format!("diri-lmr-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("holders dir");
    let logs = root.join("logs");
    let before: Vec<String> = manager_jobs().into_iter().map(|job| job.2).collect();

    // Any executable turns the launchd path on.
    // SAFETY: set before any launch; this file holds one test.
    unsafe {
        std::env::set_var(AGENT_LAUNCHER_ENV, "/usr/bin/true");
        std::env::set_var("DIRI_HOLDER_IDLE_SECONDS", "1");
    }

    let holder = std::path::Path::new(env!("CARGO_BIN_EXE_diri-holder"));
    let paths = HolderPaths::new(&root, "s_lmr");
    let spec = HolderLaunchSpec {
        session_id: paths.session_id.clone(),
        socket_path: paths.socket().to_string_lossy().into_owned(),
        pid_file_path: paths.pid_file().to_string_lossy().into_owned(),
        log_file_path: logs.join("s_lmr.bin").to_string_lossy().into_owned(),
        argv: vec!["/bin/cat".into()],
        cwd: "/tmp".into(),
        environment: HashMap::from([("PATH".to_string(), "/usr/bin:/bin".to_string())]),
        cols: 80,
        rows: 24,
        disk_capacity: DEFAULT_DISK_CAPACITY,
    };
    let started = Instant::now();
    let manager_pid = HolderLauncher::launch(holder, &paths, &spec).expect("launch");
    eprintln!("manager answered after {:?}", started.elapsed());

    let deadline = Instant::now() + Duration::from_secs(5);
    let (relay, manager) = loop {
        let new: Vec<_> = manager_jobs()
            .into_iter()
            .filter(|job| !before.contains(&job.2))
            .collect();
        let manager = new.iter().find(|job| job.0 == manager_pid.to_string());
        let relay = new.iter().find(|job| job.0 == "-");
        if let (Some(relay), Some(manager)) = (relay, manager) {
            break (relay.clone(), manager.clone());
        }
        assert!(
            Instant::now() < deadline,
            "expected a finished relay job and a running manager job, got {new:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(relay.1, "0", "the relay job exits cleanly once it relayed");
    assert!(
        manager.2 > relay.2,
        "the manager's label follows the relay's"
    );

    let client = HolderClient::new(paths.socket());
    let deadline = Instant::now() + Duration::from_secs(20);
    while !client.is_alive() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = client.kill_tree();
    eprintln!("session ended after {:?}", started.elapsed());
    for (_, _, label) in [relay, manager] {
        let _ = Command::new("/bin/launchctl")
            .args([
                "bootout",
                &format!("gui/{}/{label}", unsafe { libc::getuid() }),
            ])
            .output();
    }
    let _ = std::fs::remove_dir_all(&root);
}
