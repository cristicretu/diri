//! Bootstrapped remote PTY Helper.

mod bridge;
mod directories;
mod environment;
mod holder;
mod output_log;
mod paths;
mod persistence;
mod state;

use std::collections::HashSet;
use std::fs;
use std::io::{self, Read, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use diri_proto::remote_pty::{
    EnvironmentCaptureRequest, GcResult, HelperProbe, LaunchRequest, PHASE_ONE_HELPER_CAPABILITIES,
    PROTOCOL_MAJOR, PROTOCOL_MINOR, ProtocolVersion, RemoteProcessState, SessionInspection,
    SessionSelector,
};
use serde::Serialize;
use sha2::{Digest, Sha256};

pub use diri_proto::remote_pty::HelperProbe as ProbeReport;

pub const BUILD_ID: &str = env!("DIRI_REMOTE_EFFECTIVE_BUILD_ID");

/// Complete capability surface of the Helper binary. Holder handshakes use
/// the narrower phase-one terminal subset; `probe` advertises management
/// commands too so the Engine rejects an old binary before invoking one.
pub const HELPER_CAPABILITIES: &[diri_proto::remote_pty::RemoteCapability] =
    PHASE_ONE_HELPER_CAPABILITIES;

pub const EXIT_OK: i32 = 0;
pub const EXIT_FAILURE: i32 = 1;
pub const EXIT_USAGE: i32 = 2;

const HELP: &str = "diri-remote <command>\n\n\
Commands:\n  \
probe [--format=json]  Report platform, build, and protocol metadata\n  \
launch                 Read a structured launch request from stdin\n  \
attach                 Bridge a binary protocol stream to one Holder\n  \
inspect                Read an authenticated session selector from stdin\n  \
list                   List local Holder session facts\n  \
kill                   Stop an authenticated Holder and its process tree\n  \
gc                     Remove dead, unreferenced session state\n  \
environment            Capture the remote login/cwd environment\n  \
directories            List one bounded directory level\n  \
persistence            Run a persistence capability probe step\n  \
activate               Atomically activate this uploaded Helper build\n";

pub fn collect_probe(executable: &Path) -> io::Result<HelperProbe> {
    Ok(HelperProbe {
        protocol: ProtocolVersion {
            major: PROTOCOL_MAJOR,
            minor: PROTOCOL_MINOR,
        },
        build_id: BUILD_ID.to_string(),
        artifact_sha256: sha256_file(executable)?,
        target: target_name().to_string(),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        supported: supported_target(),
        holder_available: supported_target(),
        capabilities: HELPER_CAPABILITIES.to_vec(),
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Invocation {
    Help,
    Version,
    Probe {
        json: bool,
    },
    Launch,
    Attach,
    Inspect,
    List,
    Kill,
    Gc,
    Environment,
    Directories,
    Persistence,
    Activate,
    HiddenHolder,
    HiddenHolderFile {
        session_id: String,
        state_root: std::path::PathBuf,
    },
    HiddenProcessGuard {
        process_pid: u32,
    },
    HiddenDumpEnvironment,
    HiddenEnvironmentTestShell(std::path::PathBuf),
    HiddenPersistenceWitness,
    HiddenPersistenceWitnessArg {
        nonce: String,
        state_root: std::path::PathBuf,
    },
}

/// Execute against explicit streams so fake-SSH and protocol tests do not
/// depend on a developer's terminal or installed Helper.
pub fn execute<W: Write + Send>(
    arguments: impl IntoIterator<Item = String>,
    executable: &Path,
    stdin: &mut dyn Read,
    stdout: &mut W,
    stderr: &mut dyn Write,
) -> i32 {
    let invocation = match parse(arguments) {
        Ok(invocation) => invocation,
        Err(message) => {
            let _ = writeln!(stderr, "diri-remote: {message}");
            return EXIT_USAGE;
        }
    };

    let result = match invocation {
        Invocation::Help => stdout.write_all(HELP.as_bytes()),
        Invocation::Version => writeln!(stdout, "diri-remote {BUILD_ID}"),
        Invocation::Probe { json } => collect_probe(executable).and_then(|report| {
            if json {
                write_json(stdout, &report)
            } else {
                let mut encoded = serde_json::to_vec_pretty(&report)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                encoded.push(b'\n');
                stdout.write_all(&encoded)
            }
        }),
        Invocation::Launch => holder::read_limited_json::<_, LaunchRequest>(
            stdin,
            diri_proto::remote_pty::MAX_LAUNCH_BYTES,
        )
        .and_then(|request| holder::launch(request, executable))
        .and_then(|result| write_json(stdout, &result)),
        Invocation::Attach => bridge::run(stdin, &mut *stdout),
        Invocation::Inspect => read_selector(stdin)
            .and_then(|selector| inspect(&selector))
            .and_then(|inspection| write_json(stdout, &inspection)),
        Invocation::List => list().and_then(|sessions| write_json(stdout, &sessions)),
        Invocation::Kill => read_selector(stdin)
            .and_then(|selector| kill(&selector))
            .and_then(|inspection| write_json(stdout, &inspection)),
        Invocation::Gc => gc().and_then(|result| write_json(stdout, &result)),
        Invocation::Environment => {
            holder::read_limited_json::<_, EnvironmentCaptureRequest>(stdin, 64 * 1024)
                .and_then(|request| environment::capture(&request, executable))
                .and_then(|result| write_json(stdout, &result))
        }
        Invocation::Directories => holder::read_limited_json::<
            _,
            diri_proto::remote_pty::DirectoryListRequest,
        >(stdin, 8 * 1024)
        .and_then(|request| directories::list(&request))
        .and_then(|result| write_json(stdout, &result)),
        Invocation::Persistence => holder::read_limited_json::<
            _,
            diri_proto::remote_pty::PersistenceProbeRequest,
        >(stdin, 4096)
        .and_then(|request| persistence::execute(&request, executable))
        .and_then(|result| write_json(stdout, &result)),
        Invocation::Activate => activate(executable).and_then(|result| write_json(stdout, &result)),
        Invocation::HiddenHolder => holder::run_from_stdin(),
        Invocation::HiddenHolderFile {
            session_id,
            state_root,
        } => holder::run_from_file(&session_id, &state_root),
        Invocation::HiddenProcessGuard { process_pid } => {
            holder::run_process_guard(stdin, process_pid)
        }
        Invocation::HiddenDumpEnvironment => environment::dump(stdout),
        Invocation::HiddenEnvironmentTestShell(shell) => {
            holder::read_limited_json::<_, EnvironmentCaptureRequest>(stdin, 64 * 1024)
                .and_then(|request| environment::capture_with_shell(&request, executable, &shell))
                .and_then(|result| write_json(stdout, &result))
        }
        Invocation::HiddenPersistenceWitness => persistence::witness(stdin),
        Invocation::HiddenPersistenceWitnessArg { nonce, state_root } => {
            persistence::witness_for_nonce_at(&nonce, &state_root)
        }
    };

    match result.and_then(|()| stdout.flush()) {
        Ok(()) => EXIT_OK,
        Err(error) => {
            // Errors describe phase and category only. Launch requests,
            // environments, authentication payloads and prompts are never
            // included in Helper diagnostics.
            let _ = writeln!(stderr, "diri-remote: {error}");
            EXIT_FAILURE
        }
    }
}

fn parse(arguments: impl IntoIterator<Item = String>) -> Result<Invocation, String> {
    let mut arguments = arguments.into_iter();
    let Some(command) = arguments.next() else {
        return Err("a command is required; use --help".into());
    };
    match command.as_str() {
        "-h" | "--help" | "help" => no_more(arguments, Invocation::Help),
        "-V" | "--version" | "version" => no_more(arguments, Invocation::Version),
        "probe" => {
            let remainder: Vec<String> = arguments.collect();
            match remainder.as_slice() {
                [] => Ok(Invocation::Probe { json: false }),
                [format] if format == "--format=json" => Ok(Invocation::Probe { json: true }),
                [flag, format] if flag == "--format" && format == "json" => {
                    Ok(Invocation::Probe { json: true })
                }
                _ => Err("probe accepts only --format=json".into()),
            }
        }
        "launch" => no_more(arguments, Invocation::Launch),
        "attach" => no_more(arguments, Invocation::Attach),
        "inspect" => no_more(arguments, Invocation::Inspect),
        "list" => no_more(arguments, Invocation::List),
        "kill" => no_more(arguments, Invocation::Kill),
        "gc" => no_more(arguments, Invocation::Gc),
        "environment" => no_more(arguments, Invocation::Environment),
        "directories" => no_more(arguments, Invocation::Directories),
        "persistence" => no_more(arguments, Invocation::Persistence),
        "activate" => no_more(arguments, Invocation::Activate),
        "__holder" => no_more(arguments, Invocation::HiddenHolder),
        "__holder-file" => internal_identifier_path(arguments, |session_id, state_root| {
            Invocation::HiddenHolderFile {
                session_id,
                state_root,
            }
        }),
        "__process-guard" => {
            let remainder = arguments.collect::<Vec<_>>();
            let [process_pid] = remainder.as_slice() else {
                return Err("__process-guard requires one process pid".into());
            };
            let process_pid = process_pid
                .parse::<u32>()
                .ok()
                .filter(|pid| *pid > 1)
                .ok_or_else(|| "__process-guard pid is invalid".to_string())?;
            Ok(Invocation::HiddenProcessGuard { process_pid })
        }
        "__dump-environment" => no_more(arguments, Invocation::HiddenDumpEnvironment),
        "__environment-test-shell" if cfg!(debug_assertions) => {
            let remainder = arguments.collect::<Vec<_>>();
            let [shell] = remainder.as_slice() else {
                return Err("__environment-test-shell requires one shell path".into());
            };
            let shell = std::path::PathBuf::from(shell);
            if !shell.is_absolute() || !shell.is_file() {
                return Err("test shell must be an absolute file".into());
            }
            Ok(Invocation::HiddenEnvironmentTestShell(shell))
        }
        "__persistence-witness" => no_more(arguments, Invocation::HiddenPersistenceWitness),
        "__persistence-witness-arg" => internal_identifier_path(arguments, |nonce, state_root| {
            Invocation::HiddenPersistenceWitnessArg { nonce, state_root }
        }),
        _ => Err(format!("unknown command {command:?}; use --help")),
    }
}

fn internal_identifier_path(
    mut arguments: impl Iterator<Item = String>,
    make: impl FnOnce(String, std::path::PathBuf) -> Invocation,
) -> Result<Invocation, String> {
    let value = arguments
        .next()
        .ok_or_else(|| "an internal identifier is required".to_string())?;
    paths::validate_identifier(&value).map_err(|_| "the internal identifier is invalid")?;
    let state_root = arguments
        .next()
        .map(std::path::PathBuf::from)
        .ok_or_else(|| "an internal state root is required".to_string())?;
    if !state_root.is_absolute() {
        return Err("the internal state root must be absolute".into());
    }
    if let Some(argument) = arguments.next() {
        return Err(format!("unexpected argument {argument:?}"));
    }
    Ok(make(value, state_root))
}

fn no_more(
    mut arguments: impl Iterator<Item = String>,
    invocation: Invocation,
) -> Result<Invocation, String> {
    match arguments.next() {
        None => Ok(invocation),
        Some(argument) => Err(format!("unexpected argument {argument:?}")),
    }
}

fn read_selector(reader: &mut dyn Read) -> io::Result<SessionSelector> {
    let selector: SessionSelector = holder::read_limited_json(reader, 64 * 1024)?;
    selector
        .validate()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    Ok(selector)
}

fn inspect(selector: &SessionSelector) -> io::Result<SessionInspection> {
    let roots = paths::StatePaths::resolve()?;
    let paths = roots.session(&selector.session_id)?;
    if !state::authenticate(&paths, &selector.session_token)? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "session authentication failed",
        ));
    }
    let mut state = state::read_state(&paths.state)?;
    validate_incarnation(selector, &state)?;
    if matches!(state.process_state, RemoteProcessState::Running { .. })
        && !state::holder_lock_held(&paths.lock)?
    {
        // The Holder lock, not a reusable numeric PID, is the authority for
        // liveness. Once the lock is gone we report an unknown failure and
        // deliberately avoid signaling a process that might now own a reused
        // PID/PGID.
        state.process_state = RemoteProcessState::Exited {
            code: None,
            signal: None,
        };
        state::write_state(&paths.state, &state)?;
    }
    Ok(state.inspection())
}

fn list() -> io::Result<Vec<SessionInspection>> {
    let roots = paths::StatePaths::resolve()?;
    let mut sessions = Vec::new();
    for entry in fs::read_dir(&roots.sessions)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            continue;
        }
        let Some(session_id) = entry.file_name().to_str().map(ToOwned::to_owned) else {
            continue;
        };
        let Ok(paths) = roots.session(&session_id) else {
            continue;
        };
        if let Ok(state) = state::read_state(&paths.state) {
            sessions.push(state.inspection());
        }
    }
    sessions.sort_by(|left, right| left.session_id.cmp(&right.session_id));
    Ok(sessions)
}

fn kill(selector: &SessionSelector) -> io::Result<SessionInspection> {
    let roots = paths::StatePaths::resolve()?;
    let paths = roots.session(&selector.session_id)?;
    if !state::authenticate(&paths, &selector.session_token)? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "session authentication failed",
        ));
    }
    let mut session = state::read_state(&paths.state)?;
    validate_incarnation(selector, &session)?;
    if !state::holder_lock_held(&paths.lock)? {
        return Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "Holder is not running",
        ));
    }

    if let RemoteProcessState::Running { pid } = session.process_state {
        signal_group(pid, libc::SIGTERM);
        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline && state::process_alive(pid) {
            std::thread::sleep(Duration::from_millis(20));
        }
        if state::process_alive(pid) {
            signal_group(pid, libc::SIGKILL);
        }
        session.process_state = RemoteProcessState::Exited {
            code: None,
            signal: Some(libc::SIGTERM),
        };
    }
    state::write_state(&paths.state, &session)?;
    signal_pid(session.holder_pid, libc::SIGTERM);
    if session.persistence == diri_proto::remote_pty::PersistenceCapability::UserSupervisor {
        persistence::cleanup_holder(&session.session_id);
    }
    let holder_deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < holder_deadline && state::holder_lock_held(&paths.lock)? {
        std::thread::sleep(Duration::from_millis(10));
    }
    if state::holder_lock_held(&paths.lock)? {
        signal_pid(session.holder_pid, libc::SIGKILL);
        let killed_deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < killed_deadline && state::holder_lock_held(&paths.lock)? {
            std::thread::sleep(Duration::from_millis(10));
        }
        if state::holder_lock_held(&paths.lock)? {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Holder did not release its session lock after SIGKILL",
            ));
        }
    }
    Ok(session.inspection())
}

fn gc() -> io::Result<GcResult> {
    let roots = paths::StatePaths::resolve()?;
    let mut removed = 0;
    let mut retained = 0;
    let mut referenced_builds = HashSet::from([BUILD_ID.to_string()]);
    let mut helper_gc_safe = true;
    for entry in fs::read_dir(&roots.sessions)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            retained += 1;
            continue;
        }
        let Some(session_id) = entry.file_name().to_str().map(ToOwned::to_owned) else {
            retained += 1;
            continue;
        };
        let Ok(paths) = roots.session(&session_id) else {
            retained += 1;
            continue;
        };
        let Ok(_gc_launch_lock) = state::acquire_lock(&paths.launch_lock) else {
            retained += 1;
            continue;
        };
        if state::holder_lock_held(&paths.lock).unwrap_or(true) {
            match state::read_state(&paths.state) {
                Ok(state) => {
                    referenced_builds.insert(state.holder_build_id);
                }
                Err(_) => helper_gc_safe = false,
            }
            retained += 1;
            continue;
        }
        for path in [
            &paths.socket,
            &paths.output,
            &paths.diagnostics,
            &paths.holder_start,
            &paths.auth,
            &paths.state,
            &paths.lock,
        ] {
            match fs::symlink_metadata(path) {
                Ok(metadata) if !metadata.file_type().is_symlink() => {
                    let _ = fs::remove_file(path);
                }
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        match fs::remove_dir(&paths.root) {
            Ok(()) => removed += 1,
            Err(error) if error.kind() == io::ErrorKind::DirectoryNotEmpty => retained += 1,
            Err(error) => return Err(error),
        }
    }
    let (removed_helper_builds, retained_helper_builds) = if helper_gc_safe {
        gc_helper_builds(&referenced_builds)?
    } else {
        (0, count_helper_builds().unwrap_or(0))
    };
    Ok(GcResult {
        removed_sessions: removed,
        retained_sessions: retained,
        removed_helper_builds,
        retained_helper_builds,
    })
}

fn count_helper_builds() -> io::Result<usize> {
    let root = paths::helper_protocol_root()?;
    match fs::read_dir(root) {
        Ok(entries) => Ok(entries.flatten().count()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error),
    }
}

fn gc_helper_builds(referenced: &HashSet<String>) -> io::Result<(usize, usize)> {
    let root = paths::helper_protocol_root()?;
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok((0, 0)),
        Err(error) => return Err(error),
    };
    let mut removed = 0;
    let mut retained = 0;
    for entry in entries {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        let Some(build_id) = entry.file_name().to_str().map(ToOwned::to_owned) else {
            retained += 1;
            continue;
        };
        if metadata.file_type().is_symlink()
            || !metadata.is_dir()
            || paths::validate_identifier(&build_id).is_err()
            || referenced.contains(&build_id)
        {
            retained += 1;
            continue;
        }
        let helper = entry.path().join("diri-remote");
        match fs::symlink_metadata(&helper) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
                fs::remove_file(&helper)?;
            }
            Ok(_) => {
                retained += 1;
                continue;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        match fs::remove_dir(entry.path()) {
            Ok(()) => removed += 1,
            Err(error) if error.kind() == io::ErrorKind::DirectoryNotEmpty => retained += 1,
            Err(error) => return Err(error),
        }
    }
    Ok((removed, retained))
}

fn validate_incarnation(selector: &SessionSelector, state: &state::SessionState) -> io::Result<()> {
    if selector
        .expected_incarnation
        .as_ref()
        .is_some_and(|expected| expected != &state.session_incarnation)
    {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "session incarnation does not match",
        ))
    } else {
        Ok(())
    }
}

fn write_json(writer: &mut dyn Write, value: &impl Serialize) -> io::Result<()> {
    serde_json::to_writer(&mut *writer, value)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    writer.write_all(b"\n")
}

fn activate(executable: &Path) -> io::Result<HelperProbe> {
    use std::os::unix::fs::PermissionsExt;

    validate_activation_path(executable)?;
    let file_name = executable
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "uploaded Helper has no name")
        })?;
    if !file_name.starts_with(".tmp-")
        || file_name.len() <= 5
        || !file_name[5..]
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "activate is permitted only for a nonce upload path",
        ));
    }
    let build_dir = executable.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "upload has no build directory")
    })?;
    if build_dir.file_name().and_then(|name| name.to_str()) != Some(BUILD_ID) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "upload directory does not match the Helper build id",
        ));
    }
    let expected_protocol = format!("protocol-{PROTOCOL_MAJOR}");
    if build_dir
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        != Some(expected_protocol.as_str())
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "upload directory does not match the Helper protocol",
        ));
    }
    let final_path = build_dir.join("diri-remote");
    paths::reject_symlink(&final_path)?;
    match fs::hard_link(executable, &final_path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            if sha256_file(executable)? != sha256_file(&final_path)? {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "an incompatible Helper already occupies the build path",
                ));
            }
        }
        Err(error) => return Err(error),
    }
    fs::set_permissions(&final_path, fs::Permissions::from_mode(0o700))?;
    fs::File::open(build_dir)?.sync_all()?;
    let probe = collect_probe(&final_path)?;
    if probe.protocol != ProtocolVersion::CURRENT || probe.build_id != BUILD_ID {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "activated Helper identity does not match this process",
        ));
    }
    fs::remove_file(executable)?;
    Ok(probe)
}

fn validate_activation_path(path: &Path) -> io::Result<()> {
    use std::path::Component;

    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "uploaded Helper path must be absolute and normalized",
        ));
    }
    let mut current = Some(path);
    // The bootstrap-owned suffix is `.cache/diri/bin/protocol/build/file`.
    // HOME itself may legitimately be reached through an OS-managed symlink;
    // every component Diri creates must be a real directory/file.
    for _ in 0..6 {
        let candidate = current.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "uploaded Helper path is short",
            )
        })?;
        let metadata = fs::symlink_metadata(candidate)?;
        if metadata.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "uploaded Helper path contains a symlink",
            ));
        }
        current = candidate.parent();
    }
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "uploaded Helper is not a regular file",
        ));
    }
    Ok(())
}

fn signal_group(pid: u32, signal: i32) {
    if let Ok(pid) = libc::pid_t::try_from(pid) {
        // SAFETY: the Agent process created its own process group via
        // `setsid`; kill(2) receives integers only. Errors mean it exited.
        unsafe {
            libc::kill(-pid, signal);
        }
    }
}

fn signal_pid(pid: u32, signal: i32) {
    if let Ok(pid) = libc::pid_t::try_from(pid) {
        // SAFETY: lock ownership and authenticated state were verified before
        // selecting this Holder pid; kill(2) receives integers only.
        unsafe {
            libc::kill(pid, signal);
        }
    }
}

fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let digest = hasher.finalize();
    let mut hexadecimal = String::with_capacity(digest.len() * 2);
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    for byte in digest {
        hexadecimal.push(DIGITS[usize::from(byte >> 4)] as char);
        hexadecimal.push(DIGITS[usize::from(byte & 0x0f)] as char);
    }
    Ok(hexadecimal)
}

const fn supported_target() -> bool {
    cfg!(any(
        all(target_os = "linux", target_arch = "x86_64"),
        all(target_os = "linux", target_arch = "aarch64"),
        all(target_os = "macos", target_arch = "aarch64")
    ))
}

const fn target_name() -> &'static str {
    if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "x86_64-unknown-linux-musl"
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        "aarch64-unknown-linux-musl"
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "aarch64-apple-darwin"
    } else {
        "unsupported"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(arguments: &[&str], executable: &Path, input: &[u8]) -> (i32, String, String) {
        let mut stdin = input;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = execute(
            arguments.iter().map(ToString::to_string),
            executable,
            &mut stdin,
            &mut stdout,
            &mut stderr,
        );
        (
            code,
            String::from_utf8(stdout).expect("stdout utf8"),
            String::from_utf8(stderr).expect("stderr utf8"),
        )
    }

    #[test]
    fn json_probe_reports_the_exact_artifact_hash_and_holder_capabilities() {
        let temp = tempfile::tempdir().expect("temp");
        let executable = temp.path().join("diri-remote");
        fs::write(&executable, b"abc").expect("fixture");

        let (code, stdout, stderr) = run(&["probe", "--format=json"], &executable, b"");
        assert_eq!(code, EXIT_OK, "{stderr}");
        let report: ProbeReport = serde_json::from_str(&stdout).expect("probe json");
        assert_eq!(report.protocol, ProtocolVersion::CURRENT);
        assert_eq!(
            report.artifact_sha256,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(report.build_id, BUILD_ID);
        assert_eq!(report.supported, supported_target());
        assert_eq!(report.holder_available, supported_target());
        assert_eq!(report.capabilities, HELPER_CAPABILITIES);
    }

    #[test]
    fn invalid_arguments_are_usage_errors() {
        let (code, stdout, stderr) = run(&["probe", "--format=yaml"], Path::new("unused"), b"");
        assert_eq!(code, EXIT_USAGE);
        assert!(stdout.is_empty());
        assert!(stderr.contains("only --format=json"));
    }

    #[test]
    fn help_lists_the_complete_public_command_surface() {
        let (code, stdout, stderr) = run(&["--help"], Path::new("unused"), b"");
        assert_eq!(code, EXIT_OK, "{stderr}");
        for command in [
            "probe",
            "launch",
            "attach",
            "inspect",
            "list",
            "kill",
            "gc",
            "environment",
            "activate",
        ] {
            assert!(stdout.contains(command), "missing {command}: {stdout}");
        }
        assert!(!stdout.contains("__holder"));
    }

    #[test]
    fn activation_is_no_replace_and_concurrent_same_bytes_are_idempotent() {
        let temporary = tempfile::tempdir().expect("temp");
        let build = temporary
            .path()
            .join(format!("protocol-{PROTOCOL_MAJOR}/{BUILD_ID}"));
        fs::create_dir_all(&build).expect("build dir");
        let first = build.join(".tmp-first");
        let second = build.join(".tmp-second");
        fs::write(&first, b"same artifact").expect("first");
        fs::write(&second, b"same artifact").expect("second");
        activate(&first).expect("first activation");
        activate(&second).expect("second activation");
        assert_eq!(
            fs::read(build.join("diri-remote")).expect("final"),
            b"same artifact"
        );
        assert!(!first.exists());
        assert!(!second.exists());

        let corrupt = build.join(".tmp-corrupt");
        fs::write(&corrupt, b"different artifact").expect("corrupt");
        assert!(activate(&corrupt).is_err());
        assert_eq!(
            fs::read(build.join("diri-remote")).expect("final"),
            b"same artifact"
        );
    }
}
