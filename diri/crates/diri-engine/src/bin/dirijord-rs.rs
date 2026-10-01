//! dirijord-rs — the authoritative local Diri Engine.
//!
//! It owns local and remote session orchestration. Remote phase-one spawning,
//! reconnect and adoption are implemented here; later remote hooks, MCP,
//! migration and resource features remain explicit non-goals rather than
//! reasons to delegate remote behavior to another daemon.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use diri_engine::control::{ControlServer, InjectionConfig};
use diri_engine::detect::ManifestEngine;
use diri_engine::registry::Registry;
use diri_engine::session::HolderConfig;
use diri_proto::paths::{DirijorPaths, EXIT_WHEN_ORPHANED_FLAG};

/// How long the Engine must have had no live session and no client before it
/// retires itself. Long enough that relaunching the App, or an Agent's hook
/// reaching us between sessions, never races it.
const ORPHAN_GRACE: std::time::Duration = std::time::Duration::from_secs(10 * 60);
/// One wake a minute: the grace is ten, so precision buys nothing.
const ORPHAN_WATCH_TICK: std::time::Duration = std::time::Duration::from_secs(60);

fn main() {
    // Stamp process start on stderr: captured into dirijord.boot.log by the
    // app's launcher, and our only visibility for pre-log failures.
    eprintln!(
        "dirijord-rs: process start pid={} build=diri-engine-{}",
        std::process::id(),
        env!("CARGO_PKG_VERSION")
    );

    // The app also launches us with launchd's 256-descriptor soft limit. One
    // PTY holder, one ssh child, and one client each cost several, so a
    // working fleet blows through that within a few dozen sessions and every
    // attach after that fails before its first frame.
    let fd_limit = diri_engine::limits::raise_fd_limit();
    match &fd_limit {
        Some(limit) => eprintln!(
            "dirijord-rs: file descriptor limit soft={} hard={}",
            limit.soft,
            limit.hard_label()
        ),
        None => eprintln!("dirijord-rs: file descriptor limit could not be read"),
    }

    // The app launches us with launchd's generic SHELL and minimal PATH.
    // Normalize both from the user's account before any session snapshots the
    // inherited environment: wrapped agents must return to the user's actual
    // shell (fish/zsh/…), and that shell owns the current tool PATH.
    let user_shell = login_shell();
    // SAFETY: single-threaded startup, before any spawn.
    unsafe { std::env::set_var("SHELL", &user_shell) };
    let capture_started = std::time::Instant::now();
    let captured_path = login_path(&user_shell);
    let capture_elapsed = capture_started.elapsed();
    let path = diri_engine::local_path::search_path(captured_path.as_deref(), std::env::vars());
    // SAFETY: single-threaded startup, before any spawn.
    unsafe { std::env::set_var("PATH", &path) };

    let home = diri_platform::home_dir().expect("user home directory is unavailable");
    let app_support = DirijorPaths::app_support(&home);
    let state_dir = DirijorPaths::state_dir(&home);
    let config_dir = DirijorPaths::config_dir(&home);
    let runtime_dir = DirijorPaths::runtime_dir(&home);
    let cache_dir = DirijorPaths::cache_dir(&home);
    let logs_dir = DirijorPaths::logs_dir(&home);
    for dir in [
        app_support.as_path(),
        state_dir.as_path(),
        config_dir.as_path(),
        runtime_dir.as_path(),
        cache_dir.as_path(),
        logs_dir.as_path(),
        app_support.join("holders").as_path(),
        DirijorPaths::inject_dir(&home).as_path(),
        DirijorPaths::bin_dir(&home).as_path(),
        DirijorPaths::manifest_overrides_dir(&home).as_path(),
    ] {
        if let Err(error) = ensure_private_dir(dir) {
            eprintln!("dirijord-rs: cannot create {}: {error}", dir.display());
            std::process::exit(1);
        }
    }

    // Recording starts after the environment is final: `set_var` above must
    // run before any thread exists, and the recorder starts one.
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| diri_platform::canonicalize(exe).ok())
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    start_telemetry(&home, &state_dir, &exe_dir);
    diri_telemetry::event!(
        "engine.start",
        build = diri_telemetry::id(diri_engine::telemetry::build_id()),
        fd_soft = fd_limit.as_ref().map(|limit| limit.soft),
        exit_when_orphaned = std::env::args().any(|arg| arg == EXIT_WHEN_ORPHANED_FLAG),
    );
    diri_telemetry::event!(
        "engine.login_path",
        ok = captured_path.is_some(),
        ms = capture_elapsed,
        shell = Path::new(&user_shell)
            .file_name()
            .map(|name| diri_telemetry::id(name.to_string_lossy())),
        entries = std::env::split_paths(&path).count(),
    );

    // Singleton guard: hold an exclusive lock for our lifetime so a second
    // daemon (a relaunching app whose probe raced) exits instead of stealing
    // the live daemon's socket and orphaning its PTYs. The fd leaks on
    // purpose — it must stay open until process exit.
    let lock_path = runtime_dir.join("daemon.lock");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .unwrap_or_else(|error| {
            eprintln!("dirijord-rs: cannot open {}: {error}", lock_path.display());
            std::process::exit(1);
        });
    // SAFETY: flock on an owned fd; non-blocking probe.
    if lock.try_lock().is_err() {
        eprintln!("dirijord-rs: another daemon owns the lock — exiting");
        diri_telemetry::debug_event!("engine.duplicate_exit");
        diri_telemetry::flush(std::time::Duration::from_millis(200));
        std::process::exit(0);
    }
    std::mem::forget(lock);

    let manifest_overrides = DirijorPaths::manifest_overrides_dir(&home);
    let (engine, failed) = load_manifests(&exe_dir, &manifest_overrides);
    if !failed.is_empty() {
        eprintln!(
            "dirijord-rs: {} manifest file(s) failed to parse: {failed:?}",
            failed.len()
        );
    }
    let engine = Arc::new(engine);
    diri_telemetry::event!(
        "engine.catalog",
        manifests = engine.ids().len(),
        failed = failed.len(),
    );
    if engine.ids().is_empty() {
        // An empty catalog fails silently downstream: every agent would spawn
        // as a bare shell. Refuse loudly instead.
        eprintln!("dirijord-rs: no agent manifests found — refusing to start");
        diri_telemetry::incident!("engine.no_manifests", failed = failed.len());
        diri_telemetry::flush(std::time::Duration::from_secs(1));
        std::process::exit(1);
    }

    let holder = HolderConfig {
        holders_dir: app_support.join("holders"),
        executable: holder_executable(&exe_dir),
    };

    let mut registry = Registry::new(Arc::clone(&engine), DirijorPaths::state_file(&home));
    let load_started = std::time::Instant::now();
    let state_loaded = match load_state(&mut registry) {
        Ok(count) => {
            eprintln!("dirijord-rs: loaded {count} session record(s)");
            diri_telemetry::event!(
                "engine.state_loaded",
                records = count,
                ms = load_started.elapsed(),
            );
            true
        }
        // Quarantined: the records are safe in the `.corrupt` copy, so serving
        // from an empty table cannot overwrite them.
        Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
            eprintln!("dirijord-rs: state load: {error}");
            diri_telemetry::incident!(
                "engine.state_quarantined",
                io = diri_telemetry::io_error(&error),
            );
            false
        }
        // The file is still there but unreadable (EACCES, EIO, EMFILE...).
        // Serving would persist a table rebuilt from live holders alone over
        // every exited, archived and remote record in it. Refuse to start; the
        // app reconnects and relaunches once the cause clears.
        Err(error) => {
            eprintln!("dirijord-rs: state load: {error}; refusing to start over unread state");
            diri_telemetry::incident!(
                "engine.state_unreadable",
                io = diri_telemetry::io_error(&error),
            );
            diri_telemetry::flush(std::time::Duration::from_secs(1));
            std::process::exit(1);
        }
    };
    let restore_started = std::time::Instant::now();
    let adopted = registry.restore(&holder, &logs_dir);
    diri_telemetry::event!(
        "engine.restore",
        adopted = adopted.len(),
        records = registry.record_count(),
        live = registry.live_count(),
        ms = restore_started.elapsed(),
    );
    eprintln!(
        "dirijord-rs: adopted {} live holder session(s): {adopted:?}",
        adopted.len()
    );
    let registry = Arc::new(Mutex::new(registry));
    register_gauges(&registry);

    // Stable CLI path under App Support (same contract as Swift dirijord):
    // hooks, Codex notify, and dirijor-mcp all reference this absolute path.
    // A cargo-built dirijord-rs does not sit next to a `dirijor` binary, so
    // inventing `target/debug/dirijor` makes every MCP tools/list fail.
    let cli_path = install_cli_helpers(&exe_dir, &app_support);
    let mut server = ControlServer::new(Arc::clone(&registry), DirijorPaths::socket(&home))
        .with_logs_dir(&logs_dir)
        .with_notes_dir(app_support.join("notes"))
        .with_holder(holder)
        .with_injection(InjectionConfig {
            inject_dir: DirijorPaths::inject_dir(&home),
            cli_path,
        });
    if let Some(remote) = remote_manager(&exe_dir, &app_support) {
        server = server.with_remote(remote);
    }
    let server = Arc::new(server);
    {
        let server = Arc::clone(&server);
        diri_telemetry::register_gauge("clients", move || {
            diri_telemetry::Value::from(server.connection_count())
        });
    }
    {
        let attach = server.attach_hub();
        diri_telemetry::register_gauge("attached", move || {
            diri_telemetry::Value::from(attach.sink_count())
        });
    }
    let listener = match server.bind() {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("dirijord-rs: bind: {error}");
            if error.kind() != std::io::ErrorKind::AddrInUse {
                diri_telemetry::incident!(
                    "engine.bind_failed",
                    io = diri_telemetry::io_error(&error),
                );
                diri_telemetry::flush(std::time::Duration::from_secs(1));
            }
            // A live socket means a daemon is already serving; that is the
            // singleton working, not a failure.
            std::process::exit(if error.kind() == std::io::ErrorKind::AddrInUse {
                0
            } else {
                1
            });
        }
    };

    // Only once the socket is accepting: remote adoption is SSH-bound and must
    // never be what a client waits behind.
    server.spawn_remote_restore();
    // Notes written while no Engine ran (or before notes were Sessions) get
    // their sidebar Session. Only this singleton, now bound, may adopt.
    server.spawn_note_adoption();
    server.spawn_agent_relaunch();

    // One-shot, off the accept path: reclaim per-session files no record,
    // holder, or remote binding stands behind. Never repeated while idle.
    {
        let registry = Arc::clone(&registry);
        let logs_dir = logs_dir.clone();
        let holders_dir = app_support.join("holders");
        let bindings_dir = DirijorPaths::socket(&home).parent().map_or_else(
            || PathBuf::from("remote-bindings"),
            |dir| dir.join("remote-bindings"),
        );
        let _ = std::thread::Builder::new()
            .name("diri-orphan-sweep".into())
            .spawn(move || {
                let report = diri_engine::session_files::startup_sweep(
                    &registry,
                    state_loaded,
                    &logs_dir,
                    &holders_dir,
                    &bindings_dir,
                    &diri_engine::session_files::SweepOptions::default(),
                );
                match report {
                    Some(report) => eprintln!(
                        "dirijord-rs: orphan sweep removed {} file(s) ({} bytes) and {} recovery dir(s); kept {} referenced, {} recent; {} failed; {:?}",
                        report.removed_files,
                        report.removed_bytes,
                        report.removed_recovery_dirs,
                        report.kept_referenced,
                        report.kept_recent,
                        report.failed,
                        report.elapsed,
                    ),
                    None => eprintln!("dirijord-rs: orphan sweep skipped (no loaded records)"),
                }
            });
    }

    let _watcher = diri_engine::events::spawn_registry_watcher(
        Arc::clone(&registry),
        server.events(),
        Arc::new(AtomicBool::new(false)),
    );
    let pr_monitor_wake = server.pr_monitor_wake();
    let _governor = diri_engine::governor::spawn_governor(
        Arc::clone(&registry),
        server.events(),
        server.attach_hub(),
        pr_monitor_wake.clone(),
        server.governor_config(),
        Arc::new(AtomicBool::new(false)),
    );
    let _pr_monitor = diri_engine::pr_monitor::spawn_pr_monitor(
        Arc::clone(&registry),
        server.events(),
        server.attach_hub(),
        pr_monitor_wake,
        Arc::new(AtomicBool::new(false)),
    );
    let _persist_flusher = diri_engine::registry::spawn_persist_flusher(
        Arc::clone(&registry),
        Arc::new(AtomicBool::new(false)),
    );

    // Opt-in from the launcher. A desktop App spawns us detached and asks us
    // to go when it quits; one that was killed never asks. An Engine kept up
    // by a service manager is not given the flag and stays up while idle.
    if std::env::args().any(|arg| arg == EXIT_WHEN_ORPHANED_FLAG) {
        server.spawn_orphan_watch(ORPHAN_GRACE, ORPHAN_WATCH_TICK);
    }

    eprintln!("dirijord-rs: serving {}", server.socket_path().display());
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let server = Arc::clone(&server);
                let _ = std::thread::Builder::new()
                    .name("dirijord-connection".into())
                    .spawn(move || {
                        let _ = server.serve(stream);
                    });
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            // Never leave the accept loop: exiting here strands every
            // attached terminal, and the usual cause (descriptor exhaustion)
            // clears the moment a session or client goes away.
            Err(error) => {
                let delay = diri_engine::limits::accept_retry_delay(&error);
                eprintln!("dirijord-rs: accept: {error}; retrying in {delay:?}");
                record_accept_error(&error);
                std::thread::sleep(delay);
            }
        }
    }
}

/// Starts the flight recorder, the uploader, and the crash-report watcher.
/// Holders this Engine launches record into the same spool.
fn start_telemetry(home: &Path, state_dir: &Path, exe_dir: &Path) {
    if !diri_telemetry::init(diri_telemetry::Process::Engine, state_dir) {
        return;
    }
    diri_telemetry::install_panic_hook();
    diri_telemetry::start_health_sampler(std::time::Duration::from_secs(60));
    diri_engine::telemetry::set_holder_state_dir(state_dir);
    if let Some(endpoint) = diri_telemetry::upload::endpoint() {
        let meta = diri_engine::telemetry::upload_meta(exe_dir);
        match diri_telemetry::upload::Uploader::new(state_dir.to_path_buf(), endpoint, meta).spawn()
        {
            Ok(handle) => handle.install(),
            Err(error) => eprintln!("dirijord-rs: telemetry uploader did not start: {error}"),
        }
    }
    diri_engine::telemetry::crash_reports::spawn_watcher(
        home.to_path_buf(),
        state_dir.to_path_buf(),
    );
}

/// Session counts for every `health` event. `try_lock`: a sampler that waits
/// behind a wedged Registry would stop reporting exactly when it matters.
fn register_gauges(registry: &Arc<Mutex<Registry>>) {
    let registry = Arc::clone(registry);
    diri_telemetry::register_gauge("sessions", move || match registry.try_lock() {
        Ok(registry) => {
            let counts = registry.telemetry_counts();
            diri_telemetry::Value::Obj(vec![
                ("records", counts.records.into()),
                ("live", counts.live.into()),
                ("held", counts.held.into()),
                ("remote", counts.remote.into()),
                ("hibernated", counts.hibernated.into()),
                ("working", counts.working.into()),
                ("needs_input", counts.needs_input.into()),
            ])
        }
        Err(_) => diri_telemetry::Value::from("busy"),
    });
}

/// Accept failures are retried forever; descriptor exhaustion is the one
/// that strands every attached terminal, so it is an incident, once per
/// burst rather than once per retry.
fn record_accept_error(error: &std::io::Error) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static LAST_MS: AtomicU64 = AtomicU64::new(0);
    diri_telemetry::count("engine.accept_errors", 1);
    let now = diri_telemetry::now_ms();
    if now.saturating_sub(LAST_MS.load(Ordering::Relaxed)) < 60_000 {
        return;
    }
    LAST_MS.store(now, Ordering::Relaxed);
    #[cfg(unix)]
    let exhausted = matches!(error.raw_os_error(), Some(libc::EMFILE | libc::ENFILE));
    #[cfg(windows)]
    let exhausted = matches!(error.kind(), std::io::ErrorKind::OutOfMemory);
    if exhausted {
        diri_telemetry::incident!("engine.accept_failed", io = diri_telemetry::io_error(error));
    } else {
        diri_telemetry::error_event!("engine.accept_failed", io = diri_telemetry::io_error(error));
    }
}

/// Loads the state file, retrying transient read failures briefly. A parse
/// failure is final at once: `load` has already quarantined the file.
fn load_state(registry: &mut Registry) -> std::io::Result<usize> {
    let mut attempt = 0;
    loop {
        match registry.load() {
            Err(error) if error.kind() != std::io::ErrorKind::InvalidData && attempt < 5 => {
                attempt += 1;
                eprintln!("dirijord-rs: state load: {error}; retrying");
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            result => return result,
        }
    }
}

/// The user's real login shell from the user database. Authoritative even
/// under a desktop service, where the SHELL env var can differ from the user's
/// configured shell (a fish user's PATH lives in config.fish, which another
/// shell would never source).
#[cfg(unix)]
fn login_shell() -> String {
    // SAFETY: getpwuid returns a pointer to a static per-thread record; it is
    // read immediately and never retained.
    unsafe {
        let record = libc::getpwuid(libc::getuid());
        if !record.is_null() {
            let shell = std::ffi::CStr::from_ptr((*record).pw_shell);
            if let Ok(shell) = shell.to_str()
                && !shell.is_empty()
                && Path::new(shell).exists()
            {
                return shell.to_owned();
            }
        }
    }
    std::env::var("SHELL").unwrap_or_else(|_| default_shell().into())
}

#[cfg(target_os = "macos")]
const fn default_shell() -> &'static str {
    "/bin/zsh"
}

#[cfg(all(unix, not(target_os = "macos")))]
const fn default_shell() -> &'static str {
    "/bin/sh"
}

/// `printenv PATH` prints the real colon-separated variable regardless of
/// shell — fish stores $PATH as a space-separated list, so `echo $PATH`
/// produces garbage there — and `-i -l`
/// sources both interactive and login files, which is where agent PATHs are
/// actually configured.
///
/// Hard ceiling: wait for the shell to exit, then read stdout. On timeout,
/// SIGKILL the process group (not SIGTERM — rc files can trap that) and fall
/// back. Never block on an unbounded pipe read while the writer may still live.
#[cfg(unix)]
fn login_path(shell: &str) -> Option<String> {
    login_path_with_timeout(shell, std::time::Duration::from_secs(5))
}

#[cfg(unix)]
fn login_path_with_timeout(shell: &str, capture_timeout: std::time::Duration) -> Option<String> {
    capture_login_path(shell, &["-i", "-l", "-c", "printenv PATH"], capture_timeout)
}

#[cfg(unix)]
fn capture_login_path(
    shell: &str,
    arguments: &[&str],
    capture_timeout: std::time::Duration,
) -> Option<String> {
    use std::io::{Read, Seek};
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::time::Instant;

    let deadline = Instant::now().checked_add(capture_timeout)?;

    // A background process from an rc file can inherit stdout after its shell
    // exits. Capturing into an unlinked regular file means reading stops at the
    // current length instead of waiting for that descendant to close a pipe.
    let mut capture = anonymous_capture_file().ok()?;
    let child_stdout = capture.try_clone().ok()?;
    let mut child = unsafe {
        Command::new(shell)
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::from(child_stdout))
            .stderr(Stdio::null())
            .pre_exec(|| {
                // Own process group so trapped shells / hung children die with us.
                if libc::setpgid(0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            })
            .spawn()
    }
    .ok()?;

    if !wait_for_login_capture(&mut child, deadline) {
        return None;
    }

    capture.rewind().ok()?;
    let mut bytes = Vec::new();
    let _ = capture.take(1 << 20).read_to_end(&mut bytes);
    let stdout = String::from_utf8_lossy(&bytes);
    // Interactive shells may print a greeting; take the last line that looks
    // like a PATH.
    let path = stdout
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| line.contains('/'))
        .map(str::to_owned)?;
    Some(path)
}

#[cfg(unix)]
fn wait_for_login_capture(child: &mut std::process::Child, deadline: std::time::Instant) -> bool {
    use std::time::{Duration, Instant};
    loop {
        match child.try_wait() {
            // Observing success after the deadline is still a timeout. Return
            // directly for a reaped child: its PID must never be signaled.
            Ok(Some(status)) => return status.success() && Instant::now() < deadline,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(
                    Duration::from_millis(50)
                        .min(deadline.saturating_duration_since(Instant::now())),
                );
            }
            Ok(None) | Err(_) => break,
        }
    }

    let pid = child.id() as i32;
    // SAFETY: the unreaped child owns this process group; negative targets it.
    unsafe {
        let _ = libc::kill(-pid, libc::SIGKILL);
        let _ = libc::kill(pid, libc::SIGKILL);
    }
    let _ = child.wait();
    false
}

#[cfg(unix)]
fn anonymous_capture_file() -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    for _ in 0..8 {
        let mut nonce = [0_u8; 8];
        getrandom::fill(&mut nonce).map_err(|error| std::io::Error::other(error.to_string()))?;
        let suffix = nonce
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let path =
            std::env::temp_dir().join(format!("dirijor-path-{}-{suffix}", std::process::id()));
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => {
                std::fs::remove_file(path)?;
                return Ok(file);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not allocate a unique PATH capture file",
    ))
}

#[cfg(unix)]
fn ensure_private_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::create_dir_all(path)?;
    let mut permissions = std::fs::metadata(path)?.permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(path, permissions)
}

/// Copy `dirijor`, `dirijor-mcp`, and the CLI's manifest resource bundle into
/// App Support `bin/`, then return the stable `dirijor` path used for injection.
#[cfg(unix)]
fn install_cli_helpers(exe_dir: &Path, app_support: &Path) -> PathBuf {
    let bin_dir = app_support.join("bin");
    let _ = std::fs::create_dir_all(&bin_dir);
    for name in ["dirijor", "dirijor-mcp"] {
        let dest = bin_dir.join(name);
        let Some(source) = cli_helper_sources(exe_dir, name)
            .into_iter()
            .find(|path| is_executable(path))
        else {
            continue;
        };
        if diri_platform::canonicalize(&source).ok() == diri_platform::canonicalize(&dest).ok() {
            continue;
        }
        match install_cli_helper(&source, &dest) {
            Ok(()) => eprintln!(
                "dirijord-rs: installed helper: {} -> {}",
                source.display(),
                dest.display()
            ),
            Err(error) => eprintln!(
                "dirijord-rs: helper install failed for {name}: {error} (source {})",
                source.display()
            ),
        }
    }
    install_cli_resource_bundle(exe_dir, &bin_dir);
    let stable = bin_dir.join("dirijor");
    if is_executable(&stable) {
        stable
    } else if is_executable(&exe_dir.join("dirijor")) {
        exe_dir.join("dirijor")
    } else {
        // Last resort: PATH lookup at spawn time. Still better than a path
        // that is known not to exist beside this Engine binary.
        PathBuf::from("dirijor")
    }
}

#[cfg(unix)]
fn install_cli_helper(source: &Path, dest: &Path) -> std::io::Result<()> {
    let file_name = dest
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid helper name")
        })?;
    let staging = dest.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()));
    let _ = std::fs::remove_file(&staging);
    std::fs::copy(source, &staging)?;
    set_executable(&staging);
    if let Err(error) = std::fs::rename(&staging, dest) {
        let _ = std::fs::remove_file(&staging);
        return Err(error);
    }
    Ok(())
}

#[cfg(unix)]
fn install_cli_resource_bundle(exe_dir: &Path, bin_dir: &Path) {
    const NAME: &str = "dirijor_DirijorCore.bundle";
    let Some(source) = cli_helper_sources(exe_dir, NAME)
        .into_iter()
        .find(|path| path.is_dir())
    else {
        return;
    };
    let dest = bin_dir.join(NAME);
    if diri_platform::canonicalize(&source).ok() == diri_platform::canonicalize(&dest).ok() {
        return;
    }
    let staging = bin_dir.join(format!(".{NAME}.{}.tmp", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    if let Err(error) = copy_dir(&source, &staging) {
        let _ = std::fs::remove_dir_all(&staging);
        eprintln!(
            "dirijord-rs: helper resource install failed: {error} (source {})",
            source.display()
        );
        return;
    }
    let _ = std::fs::remove_dir_all(&dest);
    if let Err(error) = std::fs::rename(&staging, &dest) {
        let _ = std::fs::remove_dir_all(&staging);
        eprintln!("dirijord-rs: helper resource activation failed: {error}");
    }
}

#[cfg(unix)]
fn copy_dir(source: &Path, dest: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let target = dest.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else if file_type.is_file() {
            std::fs::copy(entry.path(), target)?;
        } else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "resource bundle contains a symlink or special file",
            ));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn cli_helper_sources(exe_dir: &Path, name: &str) -> Vec<PathBuf> {
    let mut sources = vec![exe_dir.join(name)];
    // cargo: <repo>/diri/target/{debug,release} → <repo>/.build/debug/<name>
    if let Some(repo) = exe_dir
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
    {
        sources.push(repo.join(".build/debug").join(name));
        sources.push(repo.join(".build/arm64-apple-macosx/debug").join(name));
    }
    if let Ok(home) = diri_platform::home_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .ok_or(std::env::VarError::NotPresent)
    {
        sources.push(
            Path::new(&home)
                .join("Applications/diri.app/Contents/Resources/bin")
                .join(name),
        );
    }
    sources.push(PathBuf::from("/Applications/diri.app/Contents/Resources/bin").join(name));
    sources
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(unix)]
fn set_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mut perms = meta.permissions();
        perms.set_mode(perms.mode() | 0o755);
        let _ = std::fs::set_permissions(path, perms);
    }
}

/// Selects exactly one Rust-owned base catalog, then applies user overrides.
/// An explicit development catalog wins; otherwise packaged builds use their
/// count-checked sibling or platform resource catalog, and loose builds fall
/// back to the source tree.
/// Base catalogs must never be merged because that could produce a catalog
/// different from the one identified when this Engine binary was built.
fn load_manifests(exe_dir: &Path, overrides: &Path) -> (ManifestEngine, Vec<String>) {
    let configured = std::env::var_os("DIRI_MANIFESTS_DIR").map(PathBuf::from);
    load_manifests_from(
        exe_dir,
        overrides,
        configured.as_deref(),
        &diri_engine::detect::bundled_manifest_dir(),
    )
}

fn load_manifests_from(
    exe_dir: &Path,
    overrides: &Path,
    configured: Option<&Path>,
    source_catalog: &Path,
) -> (ManifestEngine, Vec<String>) {
    let sibling = exe_dir.join("manifests");
    let packaged = DirijorPaths::packaged_resources(exe_dir.join("dirijord-rs")).join("manifests");
    // A configured directory that does not exist is a misconfiguration, not an
    // instruction to run without Agents: an empty catalog silently costs every
    // session its status detection and leaves the client with Terminal only.
    // Say so and continue down the normal search order.
    let configured = configured.filter(|configured| {
        configured.is_dir() || {
            eprintln!(
                "dirijord-rs: DIRI_MANIFESTS_DIR={} is not a directory; using the built-in catalog",
                configured.display()
            );
            false
        }
    });
    let base = configured.map(Path::to_path_buf).or_else(|| {
        sibling.is_dir().then_some(sibling).or_else(|| {
            packaged.is_dir().then_some(packaged).or_else(|| {
                source_catalog
                    .is_dir()
                    .then(|| source_catalog.to_path_buf())
            })
        })
    });

    let mut dirs = base.iter().map(PathBuf::as_path).collect::<Vec<_>>();
    if overrides.is_dir() {
        dirs.push(overrides);
    }
    ManifestEngine::load_dirs(&dirs).unwrap_or_else(|error| {
        eprintln!("dirijord-rs: manifest load: {error}");
        (ManifestEngine::new(Vec::new()), Vec::new())
    })
}

fn holder_executable(exe_dir: &Path) -> PathBuf {
    exe_dir.join(diri_platform::executable_name("diri-holder"))
}

fn remote_manager(
    exe_dir: &Path,
    app_support: &Path,
) -> Option<Arc<diri_engine::remote::manager::RemoteManager>> {
    use diri_engine::remote::executor::ProcessExecutor;
    use diri_engine::remote::manager::{ArtifactCatalog, RemoteManager};

    let configured = std::env::var_os("DIRI_REMOTE_HELPER_PATH").map(PathBuf::from);
    let Some(source) = resolve_remote_catalog_source(exe_dir, configured.as_deref()) else {
        eprintln!("dirijord-rs: remote transport disabled: no current Helper artifact");
        return None;
    };
    let catalog = match source {
        RemoteCatalogSource::Native(path) => ArtifactCatalog::from_native_helper(&path),
        RemoteCatalogSource::Manifest(path) => ArtifactCatalog::from_manifest(&path),
    };
    let catalog = match catalog {
        Ok(catalog) => catalog,
        Err(error) => {
            eprintln!("dirijord-rs: remote Helper catalog rejected: {error}");
            return None;
        }
    };
    let askpass = exe_dir.join(diri_platform::executable_name("diri-ssh-askpass"));
    let executor = if askpass.is_file() {
        ProcessExecutor::default().with_askpass(askpass.into_os_string())
    } else {
        eprintln!(
            "dirijord-rs: SSH UI broker is unavailable at {}; interactive authentication is disabled",
            askpass.display()
        );
        ProcessExecutor::default()
    };
    match RemoteManager::new(executor, catalog, app_support.join("ssh-control")) {
        Ok(manager) => Some(Arc::new(manager)),
        Err(error) => {
            eprintln!("dirijord-rs: remote manager initialization failed: {error}");
            None
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum RemoteCatalogSource {
    Native(PathBuf),
    Manifest(PathBuf),
}

/// Loose Cargo builds place the just-built native Helper beside the Engine,
/// while packaged apps contain only the cross-platform manifest. Prefer the
/// sibling in the former layout so an old `target/remote-helpers` directory
/// can never silently define the current development build.
fn resolve_remote_catalog_source(
    exe_dir: &Path,
    configured: Option<&Path>,
) -> Option<RemoteCatalogSource> {
    if let Some(path) = configured {
        return Some(RemoteCatalogSource::Native(path.to_path_buf()));
    }
    let sibling = exe_dir.join("diri-remote");
    if sibling.is_file() {
        return Some(RemoteCatalogSource::Native(sibling));
    }
    [
        exe_dir.join("remote-helpers/manifest.json"),
        exe_dir.join("diri-remote-helpers/manifest.json"),
    ]
    .into_iter()
    .find(|path| path.is_file())
    .map(RemoteCatalogSource::Manifest)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn login_path_capture_does_not_wait_for_a_child_that_inherited_stdout() {
        use std::time::{Duration, Instant};

        let started = Instant::now();
        let path = capture_login_path(
            "/bin/sh",
            &["-c", "/bin/sleep 5 & printf '/fixture:/usr/bin\\n'"],
            Duration::from_secs(2),
        );

        assert_eq!(path.as_deref(), Some("/fixture:/usr/bin"));
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn an_expired_capture_deadline_rejects_an_already_successful_child() {
        use std::process::Command;
        use std::time::{Duration, Instant};

        // Reap first so the test deterministically represents a delayed owner
        // observing child success only after its capture deadline has expired.
        let mut child = Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        assert!(child.wait().unwrap().success());
        let expired = Instant::now() - Duration::from_secs(1);
        assert!(!wait_for_login_capture(&mut child, expired));
        // Reaped children are never signaled. A timely observation still works.
        assert!(wait_for_login_capture(
            &mut child,
            Instant::now() + Duration::from_secs(1)
        ));
    }

    #[test]
    fn a_zero_capture_budget_never_accepts_shell_output() {
        assert!(
            capture_login_path(
                "/bin/sh",
                &["-c", "printf '/too-late:/usr/bin\\n'"],
                std::time::Duration::ZERO,
            )
            .is_none()
        );
    }

    #[test]
    fn login_path_capture_kills_a_shell_that_exceeds_the_deadline() {
        use std::time::{Duration, Instant};

        let started = Instant::now();
        let path = capture_login_path(
            "/bin/sh",
            &["-c", "/bin/sleep 5; printf '/too-late:/usr/bin\\n'"],
            Duration::from_millis(500),
        );

        assert!(path.is_none());
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn failed_login_path_capture_preserves_inherited_and_pnpm_paths() {
        for (shell, arguments) in [
            (
                "/does-not-exist/diri-test-shell",
                vec!["-c", "printenv PATH"],
            ),
            (
                "/bin/sh",
                vec!["-c", "printf '/misleading/path\\n'; exit 1"],
            ),
            ("/bin/sh", vec!["-c", "/bin/sleep 5"]),
        ] {
            let captured =
                capture_login_path(shell, &arguments, std::time::Duration::from_millis(100));
            assert!(captured.is_none());
            let path = diri_engine::local_path::search_path(
                captured.as_deref(),
                [
                    ("PATH".into(), "/inherited/node/bin:/usr/bin".into()),
                    ("PNPM_HOME".into(), "/custom/pnpm".into()),
                ],
            );
            assert!(
                path.starts_with("/inherited/node/bin:/usr/bin:/custom/pnpm/bin:/custom/pnpm:")
            );
            assert!(!path.contains("misleading"));
        }
    }

    #[test]
    fn cli_helper_replacement_keeps_the_running_inode_intact() {
        use std::io::Read;

        let temporary = tempfile::tempdir().expect("temp");
        let source = temporary.path().join("source");
        let dest = temporary.path().join("dirijor-mcp");
        std::fs::write(&source, b"new").expect("source");
        std::fs::write(&dest, b"old").expect("dest");
        let mut running = std::fs::File::open(&dest).expect("running helper");

        install_cli_helper(&source, &dest).expect("install");

        let mut old = String::new();
        running.read_to_string(&mut old).expect("old inode");
        assert_eq!(old, "old");
        assert_eq!(std::fs::read_to_string(&dest).expect("new path"), "new");
    }

    #[test]
    fn cli_resource_bundle_is_installed_without_stale_files() {
        let temporary = tempfile::tempdir().expect("temp");
        let source = temporary.path().join("source");
        let bin = temporary.path().join("bin");
        let manifests = source.join("dirijor_DirijorCore.bundle/manifests");
        std::fs::create_dir_all(&manifests).expect("source bundle");
        std::fs::write(manifests.join("cursor.json"), b"cursor").expect("cursor manifest");

        install_cli_resource_bundle(&source, &bin);
        let installed = bin.join("dirijor_DirijorCore.bundle/manifests");
        assert_eq!(
            std::fs::read(installed.join("cursor.json")).expect("installed cursor manifest"),
            b"cursor"
        );

        std::fs::remove_file(manifests.join("cursor.json")).expect("remove old manifest");
        std::fs::write(manifests.join("codex.json"), b"codex").expect("codex manifest");
        install_cli_resource_bundle(&source, &bin);
        assert!(!installed.join("cursor.json").exists());
        assert!(installed.join("codex.json").exists());
    }

    #[test]
    fn adjacent_catalog_is_not_merged_with_the_source_catalog() {
        let temporary = tempfile::tempdir().expect("temp");
        let exe_dir = temporary.path().join("bin");
        let adjacent = exe_dir.join("manifests");
        let app_support = temporary.path().join("support");
        std::fs::create_dir_all(&adjacent).expect("adjacent catalog");
        std::fs::copy(
            diri_engine::detect::bundled_manifest_dir().join("codex.json"),
            adjacent.join("codex.json"),
        )
        .expect("copy adjacent manifest");

        let source_catalog = diri_engine::detect::bundled_manifest_dir();
        let (engine, failed) = load_manifests_from(&exe_dir, &app_support, None, &source_catalog);

        assert!(failed.is_empty(), "manifests failed to load: {failed:?}");
        assert!(
            engine.manifest("codex").is_some(),
            "the adjacent packaged catalog must be selected"
        );
        assert!(
            engine.manifest("pi").is_none(),
            "a source-tree catalog must not be merged into an adjacent packaged catalog"
        );
        assert_eq!(engine.ids(), ["codex"]);
    }

    #[test]
    fn loose_build_uses_source_catalog_when_no_adjacent_catalog_exists() {
        let temporary = tempfile::tempdir().expect("temp");
        let exe_dir = temporary.path().join("bin");
        let app_support = temporary.path().join("support");
        let source_catalog = diri_engine::detect::bundled_manifest_dir();

        let (engine, failed) = load_manifests_from(&exe_dir, &app_support, None, &source_catalog);

        assert!(failed.is_empty(), "manifests failed to load: {failed:?}");
        for id in ["claude-code", "codex", "cursor", "gemini", "pi", "shell"] {
            assert!(
                engine.manifest(id).is_some(),
                "the source catalog must supply {id}"
            );
        }
    }

    #[test]
    fn a_configured_catalog_that_does_not_exist_falls_back_to_the_source_catalog() {
        let temporary = tempfile::tempdir().expect("temp");
        let exe_dir = temporary.path().join("bin");
        let app_support = temporary.path().join("support");
        let missing = temporary.path().join("typo-manifests");
        let source_catalog = diri_engine::detect::bundled_manifest_dir();

        let (engine, failed) = load_manifests_from(
            &exe_dir,
            &app_support,
            Some(missing.as_path()),
            &source_catalog,
        );

        assert!(failed.is_empty(), "manifests failed to load: {failed:?}");
        assert!(
            engine.manifest("claude-code").is_some(),
            "a stale DIRI_MANIFESTS_DIR must not strand the daemon with an empty catalog"
        );
    }

    #[test]
    fn loose_build_prefers_current_sibling_over_a_stale_catalog() {
        let temporary = tempfile::tempdir().expect("temp");
        let sibling = temporary.path().join("diri-remote");
        let stale = temporary.path().join("remote-helpers/manifest.json");
        std::fs::create_dir_all(stale.parent().expect("manifest parent")).expect("catalog dir");
        std::fs::write(&sibling, b"current").expect("sibling");
        std::fs::write(&stale, b"stale").expect("manifest");

        assert_eq!(
            resolve_remote_catalog_source(temporary.path(), None),
            Some(RemoteCatalogSource::Native(sibling))
        );
    }

    #[test]
    fn packaged_layout_uses_the_cross_platform_manifest() {
        let temporary = tempfile::tempdir().expect("temp");
        let manifest = temporary.path().join("remote-helpers/manifest.json");
        std::fs::create_dir_all(manifest.parent().expect("manifest parent")).expect("catalog dir");
        std::fs::write(&manifest, b"catalog").expect("manifest");

        assert_eq!(
            resolve_remote_catalog_source(temporary.path(), None),
            Some(RemoteCatalogSource::Manifest(manifest))
        );
    }
}

#[cfg(windows)]
fn login_shell() -> String {
    diri_platform::launch::default_shell()
}
#[cfg(windows)]
fn login_path(_: &str) -> Option<String> {
    std::env::var("PATH").ok()
}
#[cfg(windows)]
fn ensure_private_dir(path: &Path) -> std::io::Result<()> {
    diri_platform::security::private_dir_all(path)
}
#[cfg(windows)]
fn install_cli_helpers(exe_dir: &Path, _: &Path) -> PathBuf {
    // Windows holds running images open. Versioned installation directories
    // let a new build coexist with live Holders; use its adjacent CLI directly.
    exe_dir.join("dirijor.exe")
}
