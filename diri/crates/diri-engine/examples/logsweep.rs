//! Orphan-sweep measurement.
//!
//! ```sh
//! # Read-only: what the startup sweep would reclaim in a real support dir.
//! cargo run --release -p diri-engine --example logsweep -- dry-run \
//!     "$HOME/Library/Application Support/Dirijor"
//! # Sparse replica (names, sizes, mtimes only — never content) of that dir,
//! # then a real sweep of the replica, timed.
//! cargo run --release -p diri-engine --example logsweep -- simulate \
//!     "$HOME/Library/Application Support/Dirijor" /tmp/sweep-replica
//! ```
//!
//! `LOGSWEEP_DENSE=1` fills the replica with filler bytes instead of holes.
//!
//! References are the ids of `state.json`'s session records, holder sockets,
//! and remote-binding file names — what the daemon's startup sweep keeps,
//! minus in-memory state that is empty at startup anyway.

use std::collections::HashSet;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::Instant;

use diri_engine::session_files::{SweepOptions, ids_named_in, sweep_orphans};

fn referenced(support: &Path) -> HashSet<String> {
    let bytes = std::fs::read(support.join("state.json")).expect("read state.json");
    let state: serde_json::Value = serde_json::from_slice(&bytes).expect("parse state.json");
    let mut ids: HashSet<String> = state["sessions"]
        .as_array()
        .expect("sessions")
        .iter()
        .filter_map(|record| record["id"].as_str().map(str::to_owned))
        .collect();
    let records = ids.len();
    let holders =
        diri_engine::holder::paths::HolderPaths::new(&support.join("holders"), "probe").directory;
    let held = ids_named_in(&holders, &[".sock", ".pid"]);
    let bound = ids_named_in(&support.join("remote-bindings"), &[".json"]);
    eprintln!(
        "references: {records} records, {} holder ids, {} remote bindings",
        held.len(),
        bound.len()
    );
    ids.extend(held);
    ids.extend(bound);
    ids
}

fn replicate(from: &Path, to: &Path) -> (usize, u64) {
    std::fs::create_dir_all(to).expect("create replica dir");
    let mut files = 0;
    let mut bytes = 0;
    for entry in std::fs::read_dir(from).expect("read dir").flatten() {
        let metadata = entry.metadata().expect("metadata");
        let target = to.join(entry.file_name());
        if metadata.is_dir() {
            let (f, b) = replicate(&entry.path(), &target);
            files += f;
            bytes += b;
            File::open(&target)
                .and_then(|dir| dir.set_modified(metadata.modified().expect("mtime")))
                .expect("dir mtime");
        } else if metadata.is_file() {
            let mut file = File::create(&target).expect("create");
            if std::env::var_os("LOGSWEEP_DENSE").is_some() {
                // Allocated extents, so unlink cost matches real logs.
                std::io::copy(
                    &mut std::io::Read::take(std::io::repeat(0x5a), metadata.len()),
                    &mut file,
                )
                .expect("dense fill");
            } else {
                file.set_len(metadata.len()).expect("sparse length");
            }
            file.set_modified(metadata.modified().expect("mtime"))
                .expect("mtime");
            files += 1;
            bytes += metadata.len();
        }
    }
    (files, bytes)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("dry-run") => {
            let support = PathBuf::from(&args[1]);
            let references = referenced(&support);
            let report = sweep_orphans(
                &support.join("logs"),
                Some(&support.join("sessions")),
                &references,
                &SweepOptions {
                    dry_run: true,
                    ..SweepOptions::default()
                },
            );
            println!("{report:#?}");
            println!(
                "would reclaim {} files, {:.1} MB, {} recovery dirs",
                report.removed_files,
                report.removed_bytes as f64 / 1e6,
                report.removed_recovery_dirs
            );
        }
        Some("simulate") => {
            let support = PathBuf::from(&args[1]);
            let replica = PathBuf::from(&args[2]);
            let _ = std::fs::remove_dir_all(&replica);
            let (files, bytes) = replicate(&support.join("logs"), &replica.join("logs"));
            replicate(&support.join("sessions"), &replica.join("sessions"));
            eprintln!(
                "replica: {files} log files, {:.1} MB logical",
                bytes as f64 / 1e6
            );
            let references = referenced(&support);
            let started = Instant::now();
            let report = sweep_orphans(
                &replica.join("logs"),
                Some(&replica.join("sessions")),
                &references,
                &SweepOptions::default(),
            );
            let wall = started.elapsed();
            println!("{report:#?}");
            println!(
                "swept {} files, {:.1} MB, {} recovery dirs in {wall:?}",
                report.removed_files,
                report.removed_bytes as f64 / 1e6,
                report.removed_recovery_dirs
            );
        }
        _ => {
            eprintln!("usage: logsweep dry-run <support-dir> | simulate <support-dir> <replica>");
            std::process::exit(2);
        }
    }
}
