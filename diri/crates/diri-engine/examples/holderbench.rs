//! Retained memory/idle CPU after real local Holders each drain 5 MiB.
//! Run: DIRI_HOLDER_BIN=/absolute/diri-holder cargo run --release -p diri-engine --example holderbench -- 20
//! Only private fixture sessions are launched and terminated.

use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use diri_engine::holder::{
    HolderClient, HolderLaunchSpec, HolderLauncher, HolderManagerClient, HolderManagerPaths,
    HolderPaths,
};

const BYTES: usize = 5 << 20;

struct Fleet {
    clients: Vec<HolderClient>,
    manager: HolderManagerClient,
}

impl Drop for Fleet {
    fn drop(&mut self) {
        for client in &self.clients {
            let _ = client.kill_tree();
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.manager.shutdown_if_idle().is_err() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "20".into());
    if mode == "--produce" {
        let buffer = [b'x'; 64 << 10];
        let mut out = std::io::stdout().lock();
        for _ in 0..BYTES / buffer.len() {
            out.write_all(&buffer).unwrap();
        }
        out.flush().unwrap();
        std::thread::sleep(Duration::from_secs(120));
        return;
    }
    let count: usize = mode.parse().expect("session count");
    assert!((1..=64).contains(&count));
    let binary = PathBuf::from(std::env::var_os("DIRI_HOLDER_BIN").expect("DIRI_HOLDER_BIN"));
    let root = tempfile::Builder::new()
        .prefix("diri-hbench-")
        .tempdir_in("/tmp")
        .unwrap();
    let directory = root.path().join("holders");
    let manager_paths = HolderManagerPaths::new(&directory);
    let mut fleet = Fleet {
        clients: Vec::new(),
        manager: HolderManagerClient::new(manager_paths.socket()),
    };
    let producer = std::env::current_exe().unwrap();
    let started = Instant::now();
    for index in 0..count {
        let id = format!("s_bench_{index}");
        let paths = HolderPaths::new(&directory, &id);
        // Register cleanup before launch, including ambiguous launch failures.
        fleet.clients.push(HolderClient::new(paths.socket()));
        HolderLauncher::launch(
            &binary,
            &paths,
            &HolderLaunchSpec {
                session_id: id.clone(),
                socket_path: paths.socket().to_string_lossy().into_owned(),
                pid_file_path: paths.pid_file().to_string_lossy().into_owned(),
                log_file_path: root
                    .path()
                    .join("logs")
                    .join(format!("{id}.bin"))
                    .to_string_lossy()
                    .into_owned(),
                argv: vec![producer.to_string_lossy().into_owned(), "--produce".into()],
                cwd: root.path().to_string_lossy().into_owned(),
                environment: Default::default(),
                cols: 80,
                rows: 24,
                disk_capacity: 32 << 20,
            },
        )
        .unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    while !fleet
        .clients
        .iter()
        .all(|c| c.stat().is_ok_and(|s| s.log_offset >= BYTES as u64))
    {
        assert!(
            Instant::now() < deadline,
            "Holders did not drain all output"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let drain_ms = started.elapsed().as_millis();
    std::thread::sleep(Duration::from_secs(2));
    let pid = fleet.manager.ping().unwrap();
    let footprint = diri_engine::governor::footprint_of(&[pid]);
    let before = diri_engine::governor::cpu_time_of(&[pid]);
    let started = Instant::now();
    std::thread::sleep(Duration::from_secs(5));
    let after = diri_engine::governor::cpu_time_of(&[pid]);
    let cpu =
        after.checked_sub(before).unwrap() as f64 / 1e9 / started.elapsed().as_secs_f64() * 100.0;
    assert!(footprint > 0, "memory probe unavailable");
    println!(
        "{}",
        serde_json::json!({
            "holder_binary":binary,"sessions":count,"bytes_per_session":BYTES,
            "drain_ms":drain_ms,"holder_footprint_mib":footprint as f64 / 1048576.0,
            "holder_idle_cpu_percent":cpu
        })
    );
}
