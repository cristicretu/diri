//! The per-session holder: owns exactly one PTY and child tree.
//!
//! The holder has no daemon dependencies. Its entire control interface is one
//! request/response NDJSON line per unix-socket connection, and its durable
//! output interface is the [`OutputLog`] file. When the child exits the
//! holder appends an in-band [`HolderExitMarker`] to the log — so a daemon
//! that wasn't running at the time still learns how the child died — then
//! removes its control files and stops serving.

use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;

use crate::log::{DEFAULT_RING_CAPACITY, OutputLog};
use crate::pty::{Pty, PtySpec};

use super::client::HolderClient;
use super::process_tree;
use super::protocol::{
    HolderExitMarker, HolderExitReason, HolderExitStatus, HolderLaunchSpec, HolderOperation,
    HolderRequest, HolderResponse, HolderStat,
};
use super::socket;
use super::{HolderError, HolderResult};

/// One past the highest acceptable signal number, as Swift validated with
/// `NSIG`.
#[cfg(target_os = "macos")]
const MAX_SIGNAL: i32 = 32;
#[cfg(not(target_os = "macos"))]
const MAX_SIGNAL: i32 = 65;

pub struct HolderServer;

struct Shared {
    spec: HolderLaunchSpec,
    child_pid: i32,
    /// The PTY, kept for write/resize/stat access. The master stays open for
    /// the holder's whole life; closing happens when `run` returns.
    pty: Mutex<Pty>,
    log: Mutex<OutputLog>,
    /// Log tail at the moment this holder started: the boundary between prior
    /// incarnations' bytes and bytes attributable to THIS child.
    epoch_offset: u64,
    finished: AtomicBool,
    listen_fd: AtomicI32,
}

impl HolderServer {
    /// Runs the holder to completion: spawns the child, serves control
    /// requests, and returns after the child has exited and the exit marker
    /// is durably in the log.
    pub fn run(spec: HolderLaunchSpec) -> HolderResult<()> {
        // Never double-run: a second holder for the same session would
        // interleave two writers into one output log and stack a second child.
        // If a live holder already serves this socket, defer to it — bail
        // before touching the log or spawning anything.
        if HolderClient::new(&spec.socket_path).is_alive() {
            return Err(HolderError::Launch(format!(
                "a live holder already serves {}; refusing to double-run",
                spec.socket_path
            )));
        }

        let socket_path = Path::new(&spec.socket_path).to_path_buf();
        if let Some(parent) = socket_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| HolderError::io("create socket directory", error))?;
        }

        let log = open_log(&spec)?;
        // Captured before the child can produce a single byte: everything
        // below this offset predates this incarnation.
        let epoch_offset = log.tail_offset();

        let mut env: Vec<(String, String)> = spec
            .environment
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        env.sort(); // deterministic environ order, matching no one but ourselves
        let pty_spec = PtySpec {
            argv: spec.argv.clone(),
            env,
            cwd: spec.cwd.clone().into(),
            cols: spec.cols.max(2),
            rows: spec.rows.max(2),
        };
        let pty = Pty::spawn(&pty_spec).map_err(|error| HolderError::io("PTY spawn", error))?;
        let child_pid = pty.pid() as i32;

        // Nonblocking master: the reader drains in bursts, and writes bound
        // their patience with poll rather than blocking the control loop.
        // O_NONBLOCK lives on the shared file description, so setting it on
        // this dup covers every handle.
        let reader = pty
            .reader()
            .map_err(|error| HolderError::io("PTY reader", error))?;
        set_nonblocking(reader.as_raw_fd());

        let listener = socket::listen(&socket_path)?;
        // The accept loop owns this fd from here; the exit watcher closes it
        // to end the loop (see `socket::accept_raw` for why close, not just
        // shutdown).
        let listen_fd = {
            use std::os::fd::IntoRawFd;
            listener.into_raw_fd()
        };

        let shared = Arc::new(Shared {
            child_pid,
            pty: Mutex::new(pty),
            log: Mutex::new(log),
            epoch_offset,
            finished: AtomicBool::new(false),
            listen_fd: AtomicI32::new(listen_fd),
            spec,
        });

        write_pid_file(&shared.spec.pid_file_path)?;

        let pump = {
            let shared = Arc::clone(&shared);
            let mut reader = reader;
            std::thread::Builder::new()
                .name(format!("holder-pty-{}", shared.spec.session_id))
                .spawn(move || pump_pty(&shared, &mut reader))
                .map_err(|error| HolderError::io("spawn pump", error))?
        };

        {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name(format!("holder-exit-{}", shared.spec.session_id))
                .spawn(move || watch_exit(&shared, pump))
                .map_err(|error| HolderError::io("spawn exit watcher", error))?;
        }

        while let Some(mut client) =
            socket::accept_raw(listen_fd, || shared.finished.load(Ordering::SeqCst))?
        {
            let response = match socket::read_json_line::<HolderRequest>(&mut client) {
                Ok(request) => handle(&shared, &request)
                    .unwrap_or_else(|error| HolderResponse::failure(error.to_string())),
                Err(error) => HolderResponse::failure(error.to_string()),
            };
            let _ = socket::write_json_line(&mut client, &response);
        }
        Ok(())
        // `shared` unwinds here: the PTY master closes, EOFing any straggler
        // that still holds the slave.
    }
}

fn open_log(spec: &HolderLaunchSpec) -> HolderResult<OutputLog> {
    let path = Path::new(&spec.log_file_path);
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    let session = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or(&spec.session_id);
    let capacity = if spec.disk_capacity > 0 {
        spec.disk_capacity as usize
    } else {
        super::protocol::DEFAULT_DISK_CAPACITY as usize
    };
    OutputLog::open(directory, session, DEFAULT_RING_CAPACITY, capacity, false)
        .map_err(|error| HolderError::io("open output log", error))
}

/// Drains PTY output into the log until the child is gone.
///
/// The loop polls with a timeout rather than blocking so it also notices
/// `finished` — after the exit marker is written, nothing more may be
/// appended, or straggling grandchild output would land beyond the marker.
fn pump_pty(shared: &Shared, reader: &mut crate::pty::PtyStream) {
    let mut buffer = [0u8; 64 << 10];
    loop {
        if shared.finished.load(Ordering::SeqCst) {
            return;
        }
        match reader.wait_readable(Duration::from_millis(100)) {
            Ok(false) => continue,
            Ok(true) => {}
            Err(_) => return,
        }
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => return, // EOF: every slave handle is gone
                Ok(count) => {
                    // Bytes read are appended even if `finished` was just set:
                    // the exit watcher joins this thread before writing the
                    // marker, so nothing can land beyond it — but a byte
                    // consumed from the kernel and then dropped would be lost.
                    let _ = shared.log.lock().expect("log").append(&buffer[..count]);
                    if shared.finished.load(Ordering::SeqCst) {
                        return;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return,
            }
        }
    }
}

/// Reaps the child, then finishes the holder: final drain, exit marker,
/// control-file cleanup, listener shutdown.
fn watch_exit(shared: &Shared, pump: std::thread::JoinHandle<()>) {
    let mut status: libc::c_int = 0;
    // SAFETY: waitpid on our own child; EINTR retried.
    while unsafe { libc::waitpid(shared.child_pid, &mut status, 0) } < 0 {
        if std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            break;
        }
    }
    // The same decode Swift used, bit for bit, because it feeds the marker.
    let exit = if status & 0x7F != 0 {
        HolderExitStatus {
            reason: HolderExitReason::Signaled,
            code: None,
            signal: Some(status & 0x7F),
        }
    } else {
        HolderExitStatus {
            reason: HolderExitReason::Exited,
            code: Some((status >> 8) & 0xFF),
            signal: None,
        }
    };

    if shared.finished.swap(true, Ordering::SeqCst) {
        return;
    }
    // The pump must stop before the final drain, or a straggler byte could
    // land after the exit marker.
    let _ = pump.join();

    if let Ok(mut drain) = shared.pty.lock().expect("pty").reader() {
        let mut buffer = [0u8; 64 << 10];
        loop {
            match drain.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    let _ = shared.log.lock().expect("log").append(&buffer[..count]);
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break, // WouldBlock: nothing buffered
            }
        }
    }

    {
        let mut log = shared.log.lock().expect("log");
        let _ = log.append(&HolderExitMarker::encode(&exit));
        let _ = log.flush();
    }

    let _ = std::fs::remove_file(&shared.spec.socket_path);
    let _ = std::fs::remove_file(&shared.spec.pid_file_path);

    let listen_fd = shared.listen_fd.swap(-1, Ordering::SeqCst);
    if listen_fd >= 0 {
        // Shutdown then close: on macOS only the close wakes a blocked
        // accept(2) on an AF_UNIX listener. `finished` is already set, so the
        // woken loop exits rather than reporting an error.
        // SAFETY: this fd was surrendered to raw ownership in `run`; nothing
        // else closes it.
        unsafe {
            libc::shutdown(listen_fd, libc::SHUT_RDWR);
            libc::close(listen_fd);
        }
    }
}

fn handle(shared: &Shared, request: &HolderRequest) -> HolderResult<HolderResponse> {
    match request.op {
        HolderOperation::Write => {
            let data = request
                .data
                .as_deref()
                .and_then(|encoded| {
                    base64::engine::general_purpose::STANDARD
                        .decode(encoded)
                        .ok()
                })
                .ok_or_else(|| HolderError::InvalidRequest("write requires base64 data".into()))?;
            write_pty(shared, &data)?;
            Ok(HolderResponse::success())
        }

        HolderOperation::Resize => {
            let (Some(cols), Some(rows)) = (request.cols, request.rows) else {
                return Err(HolderError::InvalidRequest(
                    "resize requires cols/rows >= 2".into(),
                ));
            };
            if cols < 2 || rows < 2 {
                return Err(HolderError::InvalidRequest(
                    "resize requires cols/rows >= 2".into(),
                ));
            }
            let _ = shared.pty.lock().expect("pty").resize(cols, rows);
            Ok(HolderResponse::success())
        }

        HolderOperation::Signal => {
            let signal = request
                .sig
                .ok_or_else(|| HolderError::InvalidRequest("signal requires a valid sig".into()))?;
            if signal <= 0 || signal >= MAX_SIGNAL {
                return Err(HolderError::InvalidRequest(
                    "signal requires a valid sig".into(),
                ));
            }
            Ok(HolderResponse::with_tree(process_tree::signal(
                shared.child_pid,
                signal,
            )))
        }

        HolderOperation::KillTree => {
            process_tree::kill_tree(shared.child_pid);
            Ok(HolderResponse::success())
        }

        HolderOperation::Stat => Ok(HolderResponse::with_stat(current_stat(shared))),
    }
}

/// Writes with the same bounded semantics the Swift holder used: retry
/// `EINTR`/`EAGAIN`, waiting for the child to drain, but give up if the PTY
/// stays unwritable for a full second.
fn write_pty(shared: &Shared, data: &[u8]) -> HolderResult<()> {
    let pty = shared.pty.lock().expect("pty");
    let writer = pty
        .writer()
        .map_err(|error| HolderError::io("PTY writer", error))?;
    let fd = writer.as_raw_fd();
    let mut written = 0;
    while written < data.len() {
        // SAFETY: plain write(2) on an owned fd with an in-bounds slice.
        let count =
            unsafe { libc::write(fd, data[written..].as_ptr().cast(), data.len() - written) };
        if count > 0 {
            written += count as usize;
            continue;
        }
        let error = std::io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(libc::EAGAIN) => {
                let mut poll_fd = libc::pollfd {
                    fd,
                    events: libc::POLLOUT,
                    revents: 0,
                };
                // SAFETY: one initialized pollfd, millisecond timeout.
                let ready = unsafe { libc::poll(&mut poll_fd, 1, 1000) };
                if ready <= 0 || poll_fd.revents & libc::POLLOUT == 0 {
                    return Err(HolderError::Transport(
                        "PTY remained unwritable for 1 second".into(),
                    ));
                }
            }
            _ => return Err(HolderError::io("PTY write", error)),
        }
    }
    Ok(())
}

fn current_stat(shared: &Shared) -> HolderStat {
    let finished = shared.finished.load(Ordering::SeqCst);
    let pty = shared.pty.lock().expect("pty");
    // SAFETY: kill with signal 0 only checks existence.
    let child_alive = unsafe { libc::kill(shared.child_pid, 0) } == 0;
    let master_fd = pty.writer().map(|stream| stream.as_raw_fd()).unwrap_or(-1);
    // SAFETY: tcgetpgrp on the master; -1 fd yields an error, mapped to None.
    let foreground = unsafe { libc::tcgetpgrp(master_fd) };
    let size = pty.size().ok();
    HolderStat {
        child_pid: shared.child_pid,
        alive: !finished && child_alive,
        log_offset: shared.log.lock().expect("log").tail_offset(),
        foreground_pid: (foreground > 0).then_some(foreground),
        cols: size.map(|(cols, _)| cols),
        rows: size.map(|(_, rows)| rows),
        epoch_offset: Some(shared.epoch_offset),
    }
}

fn write_pid_file(path: &str) -> HolderResult<()> {
    let contents = format!("{}\n", std::process::id());
    std::fs::write(path, contents).map_err(|error| HolderError::io("write pid file", error))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

fn set_nonblocking(fd: i32) {
    // SAFETY: fcntl on an owned fd.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
}
