//! Cross-SSH persistence capability probe.

use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use diri_proto::remote_pty::{
    PersistenceProbeAction, PersistenceProbeRequest, PersistenceProbeResult,
};
use serde::{Deserialize, Serialize};

use crate::holder::read_limited_json;
use crate::paths::{StatePaths, create_private_file, ensure_private_dir, reject_symlink};
use crate::state::process_alive;

const WITNESS_LIFETIME: Duration = Duration::from_secs(30);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Deserialize, Serialize)]
struct WitnessStart {
    nonce: String,
}

#[derive(Deserialize, Serialize)]
struct WitnessState {
    pid: u32,
}

pub fn execute(
    request: &PersistenceProbeRequest,
    executable: &Path,
) -> io::Result<PersistenceProbeResult> {
    request
        .validate()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let path = witness_path(&request.nonce)?;
    match request.action {
        PersistenceProbeAction::BeginNative => begin(&request.nonce, &path, executable),
        PersistenceProbeAction::BeginSupervisor => begin_supervised(
            &request.nonce,
            &path,
            executable,
            &StatePaths::resolve()?.root,
        ),
        PersistenceProbeAction::Check => Ok(PersistenceProbeResult {
            alive: read_state(&path).is_ok_and(|state| process_alive(state.pid)),
        }),
        PersistenceProbeAction::Cleanup => {
            if let Ok(state) = read_state(&path)
                && process_alive(state.pid)
            {
                // SAFETY: the witness PID came from an owner-only probe file
                // for an unpredictable nonce and is checked for liveness.
                unsafe {
                    libc::kill(state.pid as i32, libc::SIGTERM);
                }
            }
            reject_symlink(&path)?;
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            cleanup_supervisor(&request.nonce);
            Ok(PersistenceProbeResult { alive: false })
        }
    }
}

fn begin(nonce: &str, path: &Path, executable: &Path) -> io::Result<PersistenceProbeResult> {
    reject_symlink(path)?;
    if path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "persistence probe nonce already exists",
        ));
    }
    let mut child = Command::new(executable);
    child
        .arg("__persistence-witness")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: only the async-signal-safe `setsid` syscall runs after fork.
    unsafe {
        child.pre_exec(|| {
            if libc::setsid() < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let mut child = child.spawn()?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "witness stdin unavailable"))?;
    serde_json::to_writer(
        &mut stdin,
        &WitnessStart {
            nonce: nonce.to_string(),
        },
    )
    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    drop(stdin);

    let deadline = Instant::now() + STARTUP_TIMEOUT;
    while Instant::now() < deadline {
        if let Ok(state) = read_state(path)
            && state.pid == child.id()
            && process_alive(state.pid)
        {
            return Ok(PersistenceProbeResult { alive: true });
        }
        if let Some(status) = child.try_wait()? {
            return Err(io::Error::other(format!(
                "persistence witness exited during startup with {status}"
            )));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = child.kill();
    let _ = child.wait();
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "persistence witness did not become ready",
    ))
}

pub fn witness(mut input: impl Read) -> io::Result<()> {
    let start: WitnessStart = read_limited_json(&mut input, 4096)?;
    witness_for_nonce(&start.nonce)
}

pub fn witness_for_nonce(nonce: &str) -> io::Result<()> {
    witness_for_nonce_at(nonce, &StatePaths::resolve()?.root)
}

pub fn witness_for_nonce_at(nonce: &str, state_root: &Path) -> io::Result<()> {
    let request = PersistenceProbeRequest {
        nonce: nonce.to_string(),
        action: PersistenceProbeAction::Check,
    };
    request
        .validate()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let path = witness_path_at(&request.nonce, state_root)?;
    let mut file = create_private_file(&path)?;
    serde_json::to_writer(
        &mut file,
        &WitnessState {
            pid: std::process::id(),
        },
    )
    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    file.flush()?;
    let deadline = Instant::now() + WITNESS_LIFETIME;
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = fs::remove_file(path);
    Ok(())
}

fn begin_supervised(
    nonce: &str,
    path: &Path,
    executable: &Path,
    state_root: &Path,
) -> io::Result<PersistenceProbeResult> {
    reject_symlink(path)?;
    if path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "persistence probe nonce already exists",
        ));
    }
    let label = supervisor_label("probe", nonce);
    if !submit_supervisor(
        &label,
        executable,
        &[
            OsString::from("__persistence-witness-arg"),
            OsString::from(nonce),
            state_root.as_os_str().to_owned(),
        ],
    )? {
        return Ok(PersistenceProbeResult { alive: false });
    }
    wait_for_witness(path, None)
}

pub fn launch_holder(executable: &Path, session_id: &str, state_root: &Path) -> io::Result<bool> {
    crate::paths::validate_identifier(session_id)?;
    submit_supervisor(
        &supervisor_label("holder", session_id),
        executable,
        &[
            OsString::from("__holder-file"),
            OsString::from(session_id),
            state_root.as_os_str().to_owned(),
        ],
    )
}

pub fn cleanup_holder(session_id: &str) {
    cleanup_supervisor_label(&supervisor_label("holder", session_id));
}

fn wait_for_witness(path: &Path, expected_pid: Option<u32>) -> io::Result<PersistenceProbeResult> {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    while Instant::now() < deadline {
        if let Ok(state) = read_state(path)
            && expected_pid.is_none_or(|pid| pid == state.pid)
            && process_alive(state.pid)
        {
            return Ok(PersistenceProbeResult { alive: true });
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(PersistenceProbeResult { alive: false })
}

fn submit_supervisor(label: &str, executable: &Path, args: &[OsString]) -> io::Result<bool> {
    #[cfg(target_os = "linux")]
    {
        let status = Command::new("systemd-run")
            .args(["--user", "--quiet", "--collect"])
            .arg(format!("--unit={label}"))
            .arg(executable)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        return match status {
            Ok(status) => Ok(status.success()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        };
    }
    #[cfg(target_os = "macos")]
    {
        let status = Command::new("launchctl")
            .args(["submit", "-l", label, "--"])
            .arg(executable)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        return match status {
            Ok(status) => Ok(status.success()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        };
    }
    #[allow(unreachable_code)]
    Ok(false)
}

fn cleanup_supervisor(nonce: &str) {
    let label = supervisor_label("probe", nonce);
    cleanup_supervisor_label(&label);
}

fn cleanup_supervisor_label(label: &str) {
    #[cfg(target_os = "linux")]
    let _ = Command::new("systemctl")
        .args(["--user", "stop", label])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    #[cfg(target_os = "macos")]
    let _ = Command::new("launchctl")
        .args(["remove", label])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn supervisor_label(kind: &str, id: &str) -> String {
    format!("diri-{kind}-{id}")
}

fn witness_path(nonce: &str) -> io::Result<PathBuf> {
    witness_path_at(nonce, &StatePaths::resolve()?.root)
}

fn witness_path_at(nonce: &str, state_root: &Path) -> io::Result<PathBuf> {
    crate::paths::validate_identifier(nonce)?;
    let roots = StatePaths::from_root(state_root.to_path_buf())?;
    let probes = roots.root.join("persistence-probes");
    ensure_private_dir(&probes)?;
    Ok(probes.join(format!("{nonce}.json")))
}

fn read_state(path: &Path) -> io::Result<WitnessState> {
    reject_symlink(path)?;
    let bytes = fs::read(path)?;
    serde_json::from_slice(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}
