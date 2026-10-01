//! The holder executable: `--manager <directory>` hosts every session holder
//! for one registry; `--spec <path>` runs a single holder directly.
//!
//! Direct/legacy `--spec` mode remains useful for compatibility tests and
//! manual recovery. Normal daemon launches go through the shared manager.

#[cfg(unix)]
use std::time::Duration;

#[cfg(unix)]
use diri_engine::holder::{HolderManagerServer, HolderServer};

#[cfg(windows)]
fn main() {
    use std::io::Read;
    let arguments: Vec<String> = std::env::args().collect();
    let result = (|| -> std::io::Result<()> {
        let spec = value_after(&arguments, "--spec").ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "usage: diri-holder --spec <path>",
            )
        })?;
        let mut bytes = Vec::new();
        diri_platform::security::read_owned(std::path::Path::new(&spec), true)?
            .take(1024 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > 1024 * 1024 {
            return Err(std::io::Error::other("oversized Holder specification"));
        }
        let parsed = serde_json::from_slice(&bytes).map_err(std::io::Error::other)?;
        std::fs::remove_file(&spec)?;
        diri_engine::holder::HolderServer::run(parsed).map_err(std::io::Error::other)
    })();
    if let Err(error) = result {
        eprintln!("diri-holder: {error}");
        std::process::exit(1);
    }
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
    if arguments.len() == 2 && arguments[1] == diri_engine::holder::guard::GROUP_GUARD_FLAG {
        // The manager's liveness guard. Termination requests are ignored so a
        // `pkill diri-holder` that takes the manager down leaves the guard to
        // clean up after it; the guard exits on its own once its pipe closes.
        // SAFETY: process-level signal setup at startup.
        unsafe {
            libc::signal(libc::SIGHUP, libc::SIG_IGN);
            libc::signal(libc::SIGINT, libc::SIG_IGN);
            libc::signal(libc::SIGTERM, libc::SIG_IGN);
        }
        let _ = diri_engine::holder::guard::run_group_guard(std::io::stdin().lock());
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
        // Only a recording Engine names a spool; a manager launched without
        // one (tests, manual recovery) records nothing.
        if let Some(state_dir) =
            value_after(&arguments, diri_engine::telemetry::HOLDER_TELEMETRY_FLAG)
            && diri_telemetry::init(
                diri_telemetry::Process::Holder,
                std::path::Path::new(&state_dir),
            )
        {
            diri_telemetry::install_panic_hook();
        }
        // The manager holds a PTY, socket and exit watcher per session; at a
        // launchd 256-descriptor soft limit it runs out long before the fleet
        // does. Raise it the way the daemon does.
        let _ = diri_engine::limits::raise_fd_limit();
        // Tests shorten the idle window so managers don't outlive them.
        let idle = std::env::var("DIRI_HOLDER_IDLE_SECONDS")
            .ok()
            .and_then(|raw| raw.parse::<f64>().ok())
            .map_or(Duration::from_secs(30), Duration::from_secs_f64);
        HolderManagerServer::new(std::path::Path::new(&directory), idle)
            .with_group_guard()
            .run()
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
        diri_telemetry::incident!(
            "holder.manager_failed",
            kind = diri_engine::telemetry::holder_error_kind(&error),
        );
        diri_telemetry::flush(Duration::from_secs(1));
        std::process::exit(1);
    }
    diri_telemetry::flush(Duration::from_secs(1));
}

fn value_after(arguments: &[String], flag: &str) -> Option<String> {
    let index = arguments.iter().position(|argument| argument == flag)?;
    arguments.get(index + 1).cloned()
}
