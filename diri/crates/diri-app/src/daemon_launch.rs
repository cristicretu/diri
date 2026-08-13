//! Launch the authoritative Rust Engine (`dirijord-rs`) bundled in `diri.app`.
//!
//! diri talks to the Engine over its owner-only Unix socket. The remote PTY
//! transport exists only in this Rust Engine, so daemon resolution must never
//! silently fall back to a legacy executable.
//!
//! A bundled Engine is content-identified on launch. When an app update
//! replaces it, the old Engine persists state and exits while Holder-owned
//! local and remote Agents keep running; the new Engine then adopts them. This
//! lets the first remote action use the new packaged Helper catalog. The daemon
//! holds an `flock` singleton, so a redundant spawn still exits instantly.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use diri_proto::paths::{DirijorPaths, ENV_SOCKET};
use diri_proto::{ControlMessage, HelloParams, HelloResult, Method, RUST_ENGINE_KIND};
use sha2::{Digest, Sha256};

/// Explicit development/test override pointing at an Engine executable.
const ENV_DAEMON_PATH: &str = "DIRIJORD_PATH";

const BOOT_LOG_FILE_NAME: &str = "dirijord-rs.boot.log";

/// Ensure a daemon is reachable at `socket_path`, spawning the bundled
/// `dirijord-rs` detached if the socket is dead.
///
/// Non-blocking: after a spawn we return immediately and let
/// [`diri_client::DaemonClient`]'s own reconnect loop (500 ms → 8 s backoff)
/// connect once the daemon's socket comes up. The UI is never blocked on
/// daemon startup.
pub fn ensure_daemon_running(socket_path: &Path) {
    // A dev/test harness that manages its own daemon exports DIRIJOR_SOCKET;
    // never spawn on top of it.
    if std::env::var_os(ENV_SOCKET).is_some() {
        return;
    }

    let boot_log = boot_log_path();
    ensure_daemon_running_with(socket_path, resolve_daemon_path(), boot_log.as_deref());
}

fn ensure_daemon_running_with(
    socket_path: &Path,
    daemon: Option<PathBuf>,
    boot_log: Option<&Path>,
) {
    match probe_daemon(socket_path) {
        Ok(hello) if hello.engine_kind.as_deref() == Some(RUST_ENGINE_KIND) => {
            let Some(daemon) = daemon.as_ref() else {
                // An externally managed Rust Engine has no local artifact to
                // compare. Keep it running rather than guessing ownership.
                return;
            };
            let expected_hash = match executable_sha256(daemon) {
                Ok(hash) => hash,
                Err(error) => {
                    eprintln!(
                        "diri: cannot verify bundled daemon {}: {error}; keeping the live Engine",
                        daemon.display()
                    );
                    return;
                }
            };
            if !daemon_needs_refresh(&hello, &expected_hash) {
                return;
            }
            eprintln!(
                "diri: refreshing Rust Engine {:?} from bundled executable {}",
                hello.build,
                daemon.display()
            );
            if let Err(error) = stop_daemon_for_upgrade(socket_path, Some(hello.pid)) {
                eprintln!(
                    "diri: could not stop the outdated Rust Engine at {}: {error}",
                    socket_path.display()
                );
                return;
            }
        }
        Ok(hello) => {
            // An older release left its own daemon owning this socket, and it
            // deliberately outlives the app that started it. Refusing here
            // would strand every upgrading user: the socket stays held, no
            // Engine is ever spawned, and the app comes up empty with the
            // explanation on a stderr a bundled app never shows. Retire it the
            // same way an outdated Rust Engine is retired — `daemon.shutdown`
            // persists state first, and holder-owned sessions outlive it and
            // are re-adopted.
            eprintln!(
                "diri: replacing non-Rust daemon build {:?} at {} with the bundled Rust Engine",
                hello.build,
                socket_path.display()
            );
            if daemon.is_none() {
                eprintln!(
                    "diri: no bundled Engine to replace it with; leaving {} alone",
                    socket_path.display()
                );
                return;
            }
            if let Err(error) = stop_daemon_for_upgrade(socket_path, Some(hello.pid)) {
                eprintln!(
                    "diri: could not stop the previous daemon at {}: {error}",
                    socket_path.display()
                );
                return;
            }
        }
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound
                    | io::ErrorKind::ConnectionRefused
                    | io::ErrorKind::ConnectionReset
            ) => {}
        // Something is listening but cannot identify itself. The usual cause is
        // not corruption, it is age: every Engine older than `daemon.hello`
        // answers `not_found`, which arrives here as a plain error rather than
        // a refused connection. Returning would strand exactly the people who
        // are upgrading — the shipped 0.4.7 Engine behaves this way, verified
        // against a live one — so an Engine that cannot answer the probe is
        // retired like one that answers with the wrong build.
        //
        // Probed twice before deciding: a healthy but momentarily busy Engine
        // can miss the one-second read timeout, and restarting it over a
        // hiccup is needless churn. `daemon.shutdown` persists state first and
        // holder-owned sessions outlive it either way.
        Err(error) if probe_daemon(socket_path).is_err() => {
            eprintln!(
                "diri: replacing the Engine at {} — it could not answer the identity probe ({error})",
                socket_path.display()
            );
            if daemon.is_none() {
                eprintln!("diri: no bundled Engine to replace it with; leaving it alone");
                return;
            }
            if let Err(error) = stop_daemon_for_upgrade(socket_path, None) {
                eprintln!(
                    "diri: could not stop the unidentified Engine at {}: {error}",
                    socket_path.display()
                );
                return;
            }
        }
        Err(error) => {
            eprintln!(
                "diri: the Engine at {} answered a retried identity probe; leaving it alone ({error})",
                socket_path.display()
            );
            return;
        }
    }

    match daemon {
        Some(daemon) => match spawn_detached(&daemon, boot_log) {
            Ok(()) => eprintln!("diri: launched bundled daemon at {}", daemon.display()),
            Err(err) => {
                eprintln!(
                    "diri: failed to launch bundled daemon {}: {err}",
                    daemon.display()
                );
            }
        },
        None => eprintln!(
            "diri: no bundled dirijord-rs found next to the executable; \
             relying on an externally managed daemon"
        ),
    }
}

fn daemon_needs_refresh(hello: &HelloResult, expected_hash: &str) -> bool {
    hello.executable_hash.as_deref() != Some(expected_hash)
}

fn stop_daemon_for_upgrade(socket_path: &Path, pid: Option<i32>) -> io::Result<()> {
    control_request(socket_path, 2, Method::DAEMON_SHUTDOWN, None)?;
    for _ in 0..30 {
        if !socket_is_live(socket_path) {
            if let Some(pid) = pid {
                if !process_is_alive(pid) {
                    return Ok(());
                }
            } else {
                // Old Engines that predate Hello do not reveal a pid. Give
                // their singleton lock one scheduling turn after the socket is
                // removed, then verify that the endpoint stayed down.
                std::thread::sleep(Duration::from_millis(100));
                if !socket_is_live(socket_path) {
                    return Ok(());
                }
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "the outdated Engine did not release its socket within 3 seconds",
    ))
}

fn process_is_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal 0 does not deliver a signal; it only probes whether this
    // process id still exists and is visible to the current user.
    let result = unsafe { libc::kill(pid, 0) };
    result == 0 || io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

fn executable_sha256(path: &Path) -> io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// True when something is listening on the daemon socket right now.
fn socket_is_live(socket_path: &Path) -> bool {
    UnixStream::connect(socket_path).is_ok()
}

fn probe_daemon(socket_path: &Path) -> io::Result<HelloResult> {
    let params =
        serde_json::to_value(HelloParams::new("diri-launch-probe")).map_err(io::Error::other)?;
    let value = control_request(socket_path, 1, Method::HELLO, Some(params))?;
    serde_json::from_value(value).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("daemon Hello response is invalid: {error}"),
        )
    })
}

fn control_request(
    socket_path: &Path,
    id: u64,
    method: &str,
    params: Option<serde_json::Value>,
) -> io::Result<serde_json::Value> {
    let mut stream = UnixStream::connect(socket_path)?;
    stream.set_read_timeout(Some(Duration::from_secs(1)))?;
    stream.set_write_timeout(Some(Duration::from_secs(1)))?;
    let request = ControlMessage::Request {
        id,
        method: method.to_string(),
        params,
    };
    serde_json::to_writer(&mut stream, &request).map_err(io::Error::other)?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut response = Vec::new();
    reader
        .by_ref()
        .take(diri_proto::control::MAX_CONTROL_LINE_BYTES as u64 + 1)
        .read_until(b'\n', &mut response)?;
    if response.len() > diri_proto::control::MAX_CONTROL_LINE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "daemon response exceeds the control line limit",
        ));
    }
    let message: ControlMessage = serde_json::from_slice(&response).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("daemon response is invalid: {error}"),
        )
    })?;
    match message {
        ControlMessage::Response {
            id: response_id,
            result,
        } if response_id == id => result.map_err(io::Error::other),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "daemon returned the wrong control response",
        )),
    }
}

/// Resolve the Rust Engine executable to launch, using the live process layout.
pub fn resolve_daemon_path() -> Option<PathBuf> {
    resolve_daemon_path_from(
        std::env::var_os(ENV_DAEMON_PATH).map(PathBuf::from),
        std::env::current_exe().ok(),
        std::env::current_dir().ok(),
    )
}

/// Pure resolver, split out so the bundle layout can be unit-tested without a
/// real `diri.app`.
///
/// Search order (first executable wins):
///   1. `DIRIJORD_PATH` override (dev/tests).
///   2. Bundled: `Contents/MacOS/diri` → `../Resources/bin/dirijord-rs`.
///   3. Next to the executable (loose dev copy).
///   4. Cargo build outputs under the working dir: `target/{release,debug}/dirijord-rs`.
fn resolve_daemon_path_from(
    env_override: Option<PathBuf>,
    current_exe: Option<PathBuf>,
    current_dir: Option<PathBuf>,
) -> Option<PathBuf> {
    if let Some(path) = env_override
        && is_executable(&path)
    {
        return Some(path);
    }

    let mut candidates: Vec<PathBuf> = Vec::new();

    if let Some(exe) = current_exe
        && let Some(macos_dir) = exe.parent()
    {
        // Contents/MacOS/diri → Contents/Resources/bin/dirijord-rs
        if let Some(contents) = macos_dir.parent() {
            candidates.push(contents.join("Resources/bin/dirijord-rs"));
        }
        // Loose copy sitting right next to the executable.
        candidates.push(macos_dir.join("dirijord-rs"));
    }

    if let Some(cwd) = current_dir {
        candidates.push(cwd.join("target/release/dirijord-rs"));
        candidates.push(cwd.join("target/debug/dirijord-rs"));
    }

    candidates.into_iter().find(|path| is_executable(path))
}

/// Spawn the Engine in its own process group so it outlives diri, with
/// stdout/stderr appended to `dirijord-rs.boot.log`. We never wait on the
/// child: the daemon is meant to run independently.
fn spawn_detached(daemon: &Path, boot_log: Option<&Path>) -> io::Result<()> {
    let mut command = Command::new(daemon);
    command.stdin(Stdio::null());

    match boot_log {
        Some(log_path) => {
            let out = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(log_path)?;
            let err = out.try_clone()?;
            command.stdout(Stdio::from(out)).stderr(Stdio::from(err));
        }
        None => {
            command.stdout(Stdio::null()).stderr(Stdio::null());
        }
    }

    // New process group (setpgid to the child's own pid): decouples the daemon
    // from diri's signal/terminal group so quitting diri never SIGHUPs the
    // daemon or its PTYs. Equivalent intent to the Swift POSIX_SPAWN_SETSID path.
    command.process_group(0);

    // Spawn and deliberately drop the handle — we do not (and must not) wait.
    command.spawn().map(|_child| ())
}

/// The platform log directory's early-boot log, created before the Engine can
/// initialize its own diagnostics. Returns `None` when `HOME` is unset.
fn boot_log_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let logs = DirijorPaths::logs_dir(PathBuf::from(home));
    std::fs::create_dir_all(&logs).ok()?;
    Some(logs.join(BOOT_LOG_FILE_NAME))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;
    use std::sync::{Arc, Mutex};

    fn touch_executable(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"#!/bin/sh\n").unwrap();
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).unwrap();
    }

    fn serve_control(
        socket: &Path,
        responses: Vec<serde_json::Value>,
    ) -> std::thread::JoinHandle<Vec<String>> {
        let listener = UnixListener::bind(socket).expect("bind fixture daemon");
        let methods = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&methods);
        std::thread::spawn(move || {
            for result in responses {
                let (mut stream, _) = listener.accept().expect("accept fixture request");
                let mut request = String::new();
                BufReader::new(stream.try_clone().expect("clone fixture stream"))
                    .read_line(&mut request)
                    .expect("read fixture request");
                let request: ControlMessage =
                    serde_json::from_str(&request).expect("decode fixture request");
                let (id, method) = match request {
                    ControlMessage::Request { id, method, .. } => (id, method),
                    other => panic!("unexpected fixture message: {other:?}"),
                };
                recorded.lock().expect("methods").push(method);
                serde_json::to_writer(
                    &mut stream,
                    &ControlMessage::Response {
                        id,
                        result: Ok(result),
                    },
                )
                .expect("write fixture response");
                stream.write_all(b"\n").expect("terminate fixture response");
            }
            recorded.lock().expect("methods").clone()
        })
    }

    #[test]
    fn rust_engine_identity_is_read_from_the_live_socket() {
        let tmp = tempfile::tempdir().unwrap();
        let socket = tmp.path().join("daemon.sock");
        let server = serve_control(
            &socket,
            vec![serde_json::json!({
                "proto": diri_proto::WIRE_VERSION,
                "build": "fixture",
                "pid": 42,
                "engineKind": RUST_ENGINE_KIND
            })],
        );

        let hello = probe_daemon(&socket).expect("probe Rust Engine");
        assert_eq!(hello.engine_kind.as_deref(), Some(RUST_ENGINE_KIND));
        assert_eq!(server.join().expect("fixture server"), vec![Method::HELLO]);
    }

    #[test]
    fn daemon_refresh_requires_an_exact_executable_hash() {
        let hello = |hash: Option<&str>| {
            serde_json::from_value::<HelloResult>(serde_json::json!({
                "proto": diri_proto::WIRE_VERSION,
                "build": "fixture",
                "pid": 42,
                "engineKind": RUST_ENGINE_KIND,
                "executableHash": hash,
            }))
            .expect("hello")
        };

        assert!(!daemon_needs_refresh(&hello(Some("current")), "current"));
        assert!(daemon_needs_refresh(&hello(Some("stale")), "current"));
        assert!(daemon_needs_refresh(&hello(None), "current"));
    }

    #[test]
    fn executable_hash_is_streamed_as_sha256() {
        let tmp = tempfile::tempdir().unwrap();
        let executable = tmp.path().join("engine");
        std::fs::write(&executable, b"abc").unwrap();
        assert_eq!(
            executable_sha256(&executable).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn upgrade_shutdown_uses_the_persisting_daemon_rpc() {
        let tmp = tempfile::tempdir().unwrap();
        let socket = tmp.path().join("daemon.sock");
        let server = serve_control(&socket, vec![serde_json::json!({})]);

        stop_daemon_for_upgrade(&socket, None).expect("fixture daemon releases its listener");
        assert_eq!(
            server.join().expect("fixture server"),
            vec![Method::DAEMON_SHUTDOWN]
        );
    }

    #[test]
    fn an_outdated_live_daemon_is_replaced_by_the_resolved_bundle() {
        let tmp = tempfile::tempdir().unwrap();
        let socket = tmp.path().join("daemon.sock");
        let marker = tmp.path().join("new-daemon-launched");
        let daemon = tmp.path().join("dirijord-rs");
        std::fs::write(
            &daemon,
            format!("#!/bin/sh\nprintf launched > '{}'\n", marker.display()),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&daemon).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&daemon, permissions).unwrap();

        let server = serve_control(
            &socket,
            vec![
                serde_json::json!({
                    "proto": diri_proto::WIRE_VERSION,
                    "build": "outdated",
                    "pid": i32::MAX,
                    "engineKind": RUST_ENGINE_KIND,
                    "executableHash": "old-bytes",
                }),
                serde_json::json!({}),
            ],
        );

        ensure_daemon_running_with(&socket, Some(daemon), None);
        assert_eq!(
            server.join().expect("fixture server"),
            vec![Method::HELLO, Method::DAEMON_SHUTDOWN]
        );
        // Wait on a deadline, not a fixed iteration count. This is a real
        // spawned process writing a real file, and 50 × 10ms only ever bought
        // half a second — enough on an idle machine, not enough on a loaded CI
        // runner, where this failed with the marker simply not written yet.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !marker.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            std::fs::read_to_string(marker).expect("new daemon marker"),
            "launched"
        );
    }

    #[test]
    fn resolves_bundled_daemon_from_contents_macos() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let exe = root.join("Contents/MacOS/diri");
        touch_executable(&exe);
        let daemon = root.join("Contents/Resources/bin/dirijord-rs");
        touch_executable(&daemon);

        let resolved =
            resolve_daemon_path_from(None, Some(exe), None).expect("bundled daemon should resolve");
        assert_eq!(
            std::fs::canonicalize(resolved).unwrap(),
            std::fs::canonicalize(daemon).unwrap(),
        );
    }

    #[test]
    fn env_override_wins_when_executable() {
        let tmp = tempfile::tempdir().unwrap();
        let override_path = tmp.path().join("custom/dirijord-rs");
        touch_executable(&override_path);

        let resolved = resolve_daemon_path_from(Some(override_path.clone()), None, None).unwrap();
        assert_eq!(resolved, override_path);
    }

    #[test]
    fn ignores_non_executable_override_and_falls_back_next_to_exe() {
        let tmp = tempfile::tempdir().unwrap();
        // A non-executable override must be skipped.
        let bad_override = tmp.path().join("not-exec");
        std::fs::write(&bad_override, b"plain").unwrap();

        let exe = tmp.path().join("bin/diri");
        touch_executable(&exe);
        let sibling = tmp.path().join("bin/dirijord-rs");
        touch_executable(&sibling);

        let resolved = resolve_daemon_path_from(Some(bad_override), Some(exe), None).unwrap();
        assert_eq!(
            std::fs::canonicalize(resolved).unwrap(),
            std::fs::canonicalize(sibling).unwrap(),
        );
    }

    #[test]
    fn returns_none_when_nothing_resolves() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("Contents/MacOS/diri");
        touch_executable(&exe);
        // No daemon anywhere; cwd points at an empty dir.
        assert!(
            resolve_daemon_path_from(None, Some(exe), Some(tmp.path().to_path_buf())).is_none()
        );
    }
}
