//! The holder executable: `--manager <directory>` hosts every session holder
//! for one registry; `--spec <path>` runs a single holder directly.
//!
//! Direct/legacy `--spec` mode remains useful for compatibility tests and
//! manual recovery. Normal daemon launches go through the shared manager.

#[cfg(unix)]
use std::time::Duration;

#[cfg(unix)]
use diri_engine::holder::{HolderManagerServer, HolderServer};

#[cfg(not(unix))]
fn main() {
    eprintln!("diri-holder requires a unix platform");
    std::process::exit(64);
}

#[cfg(unix)]
fn main() {
    let arguments: Vec<String> = std::env::args().collect();
    if arguments
        .get(1)
        .is_some_and(|value| value == diri_pty::process_facts::account::WORKER_FLAG)
    {
        // One-shot directory-service worker: no detachment, manager, PTY or
        // Holder sockets. Its caller owns the deadline and reaps this process.
        let result = if arguments.len() == 3 {
            diri_pty::process_facts::account::parse_uid(&arguments[2]).and_then(|uid| {
                diri_pty::process_facts::account::run_worker(uid, &mut std::io::stdout())
            })
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "account worker expects one UID",
            ))
        };
        if result.is_err() {
            std::process::exit(1);
        }
        return;
    }
    // The daemon detaches us with setsid at spawn. Direct/manual launches
    // detach here as well; parent death never terminates a POSIX child, and
    // ignoring SIGHUP severs the last terminal coupling.
    // SAFETY: process-level session and signal setup at startup.
    unsafe {
        if libc::getsid(0) != libc::getpid() {
            libc::setsid();
        }
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
    }

    let result = if let Some(directory) = value_after(&arguments, "--manager") {
        // Tests shorten the idle window so managers don't outlive them.
        let idle = std::env::var("DIRI_HOLDER_IDLE_SECONDS")
            .ok()
            .and_then(|raw| raw.parse::<f64>().ok())
            .map_or(Duration::from_secs(30), Duration::from_secs_f64);
        HolderManagerServer::new(std::path::Path::new(&directory), idle).run()
    } else if let Some(spec_path) = value_after(&arguments, "--spec") {
        match std::fs::read(&spec_path) {
            Ok(data) => {
                let _ = std::fs::remove_file(&spec_path);
                match serde_json::from_slice(&data) {
                    Ok(spec) => HolderServer::run(spec),
                    Err(error) => {
                        eprintln!("diri-holder: spec did not parse: {error}");
                        std::process::exit(1);
                    }
                }
            }
            Err(error) => {
                eprintln!("diri-holder: read {spec_path}: {error}");
                std::process::exit(1);
            }
        }
    } else {
        eprintln!("usage: diri-holder --manager <directory> | --spec <path>");
        std::process::exit(64);
    };

    if let Err(error) = result {
        eprintln!("diri-holder: {error}");
        std::process::exit(1);
    }
}

fn value_after(arguments: &[String], flag: &str) -> Option<String> {
    let index = arguments.iter().position(|argument| argument == flag)?;
    arguments.get(index + 1).cloned()
}
