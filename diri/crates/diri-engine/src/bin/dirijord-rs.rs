//! dirijord-rs — the authoritative local Diri Engine.
//!
//! It owns local and remote session orchestration. Remote phase-one spawning,
//! reconnect and adoption are implemented here; later remote hooks, MCP,
//! migration and resource features remain explicit non-goals rather than
//! reasons to delegate remote behavior to another daemon.

#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::sync::atomic::AtomicBool;
#[cfg(unix)]
use std::sync::{Arc, Mutex};

#[cfg(unix)]
use diri_engine::control::{ControlServer, InjectionConfig};
#[cfg(unix)]
use diri_engine::detect::ManifestEngine;
#[cfg(unix)]
use diri_engine::registry::Registry;
#[cfg(unix)]
use diri_engine::session::HolderConfig;

#[cfg(not(unix))]
fn main() {
    eprintln!("dirijord-rs requires a unix platform");
    std::process::exit(64);
}

#[cfg(unix)]
fn main() {
    // Stamp process start on stderr: captured into dirijord.boot.log by the
    // app's launcher, and our only visibility for pre-log failures.
    eprintln!(
        "dirijord-rs: process start pid={} build=diri-engine-{}",
        std::process::id(),
        env!("CARGO_PKG_VERSION")
    );

    // The app launches us with launchd's generic SHELL and minimal PATH.
    // Normalize both from the user's account before any session snapshots the
    // inherited environment: wrapped agents must return to the user's actual
    // shell (fish/zsh/…), and that shell owns the current tool PATH.
    let user_shell = login_shell();
    // SAFETY: single-threaded startup, before any spawn.
    unsafe { std::env::set_var("SHELL", &user_shell) };
    if let Some(path) = login_path(&user_shell) {
        // SAFETY: single-threaded startup, before any spawn.
        unsafe { std::env::set_var("PATH", &path) };
    }

    let app_support = app_support_dir();
    for dir in ["logs", "holders", "inject", "bin"] {
        let _ = std::fs::create_dir_all(app_support.join(dir));
    }

    // Singleton guard: hold an exclusive lock for our lifetime so a second
    // daemon (a relaunching app whose probe raced) exits instead of stealing
    // the live daemon's socket and orphaning its PTYs. The fd leaks on
    // purpose — it must stay open until process exit.
    let lock_path = app_support.join("daemon.lock");
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
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        eprintln!("dirijord-rs: another daemon owns the lock — exiting");
        std::process::exit(0);
    }
    std::mem::forget(lock);

    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.canonicalize().ok())
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));

    let (engine, failed) = load_manifests(&exe_dir, &app_support);
    if !failed.is_empty() {
        eprintln!(
            "dirijord-rs: {} manifest file(s) failed to parse: {failed:?}",
            failed.len()
        );
    }
    let engine = Arc::new(engine);
    if engine.ids().is_empty() {
        // An empty catalog fails silently downstream: every agent would spawn
        // as a bare shell. Refuse loudly instead.
        eprintln!("dirijord-rs: no agent manifests found — refusing to start");
        std::process::exit(1);
    }

    let logs_dir = app_support.join("logs");
    let holder = HolderConfig {
        holders_dir: app_support.join("holders"),
        executable: holder_executable(&exe_dir),
    };

    let mut registry = Registry::new(Arc::clone(&engine), app_support.join("state.json"));
    match registry.load() {
        Ok(count) => eprintln!("dirijord-rs: loaded {count} session record(s)"),
        Err(error) => eprintln!("dirijord-rs: state load: {error}"),
    }
    let adopted = registry.restore(&holder, &logs_dir);
    eprintln!(
        "dirijord-rs: adopted {} live holder session(s): {adopted:?}",
        adopted.len()
    );
    let registry = Arc::new(Mutex::new(registry));

    let cli_path = exe_dir.join("dirijor");
    let mut server = ControlServer::new(Arc::clone(&registry), app_support.join("daemon.sock"))
        .with_logs_dir(&logs_dir)
        .with_holder(holder)
        .with_injection(InjectionConfig {
            inject_dir: app_support.join("inject"),
            cli_path,
        });
    if let Some(remote) = remote_manager(&exe_dir, &app_support) {
        server = server.with_remote(remote);
    }
    let server = Arc::new(server);
    let listener = match server.bind() {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("dirijord-rs: bind: {error}");
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
            Err(error) => {
                eprintln!("dirijord-rs: accept: {error}");
                break;
            }
        }
    }
}

/// The user's real login shell from the user database. Authoritative even
/// under launchd, where the SHELL env var is often /bin/zsh regardless of the
/// user's configured shell (a fish user's PATH lives in config.fish, which
/// zsh would never source).
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
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into())
}

/// Mirrors the Swift daemon's `LoginEnvironment`: `printenv PATH` prints the
/// real colon-separated variable regardless of shell — fish stores $PATH as a
/// space-separated list, so `echo $PATH` produces garbage there — and `-i -l`
/// sources both interactive and login files, which is where agent PATHs are
/// actually configured.
#[cfg(unix)]
fn login_path(shell: &str) -> Option<String> {
    let fallback = || {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        format!("{home}/.local/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin")
    };
    let output = std::process::Command::new(shell)
        .args(["-i", "-l", "-c", "printenv PATH"])
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Interactive shells may print a greeting; take the last line that looks
    // like a PATH.
    let path = stdout
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| line.contains('/'))
        .map(str::to_owned)?;
    if path.is_empty() {
        return Some(fallback());
    }
    // A single-entry answer smells like a broken profile: keep it, but append
    // the standard locations so spawns still work.
    Some(if path.contains(':') {
        path
    } else {
        format!("{path}:{}", fallback())
    })
}

#[cfg(unix)]
fn app_support_dir() -> PathBuf {
    if let Ok(root) = std::env::var("DIRIJOR_APP_SUPPORT") {
        return PathBuf::from(root);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    Path::new(&home).join("Library/Application Support/Dirijor")
}

#[cfg(unix)]
/// Rust-owned base catalog next to the executable, then user overrides, then
/// the source-tree fallback used by loose development binaries.
fn load_manifests(exe_dir: &Path, app_support: &Path) -> (ManifestEngine, Vec<String>) {
    let mut bases: Vec<PathBuf> = Vec::new();
    if let Ok(configured) = std::env::var("DIRI_MANIFESTS_DIR") {
        bases.push(PathBuf::from(configured));
    }
    bases.push(exe_dir.join("manifests"));
    bases.push(diri_engine::detect::bundled_manifest_dir());
    let base = bases.into_iter().find(|dir| dir.is_dir());
    let overrides = app_support.join("manifests/overrides");

    let mut dirs: Vec<&Path> = Vec::new();
    if let Some(base) = &base {
        dirs.push(base);
    }
    dirs.push(&overrides);
    ManifestEngine::load_dirs(&dirs).unwrap_or_else(|error| {
        eprintln!("dirijord-rs: manifest load: {error}");
        (ManifestEngine::new(Vec::new()), Vec::new())
    })
}

#[cfg(unix)]
fn holder_executable(exe_dir: &Path) -> PathBuf {
    exe_dir.join("diri-holder")
}

#[cfg(unix)]
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
    let askpass = exe_dir.join("diri-ssh-askpass");
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

#[cfg(unix)]
#[derive(Clone, Debug, Eq, PartialEq)]
enum RemoteCatalogSource {
    Native(PathBuf),
    Manifest(PathBuf),
}

/// Loose Cargo builds place the just-built native Helper beside the Engine,
/// while packaged apps contain only the cross-platform manifest. Prefer the
/// sibling in the former layout so an old `target/remote-helpers` directory
/// can never silently define the current development build.
#[cfg(unix)]
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
