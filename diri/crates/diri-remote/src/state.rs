use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::Path;

use diri_proto::remote_pty::{
    LaunchRequest, PersistenceCapability, RemoteProcessState, SessionInspection, SessionToken,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::BUILD_ID;
use crate::paths::{SessionPaths, create_private_file, open_private_file, reject_symlink};

const STATE_SCHEMA: u16 = 1;
const MAX_STATE_BYTES: u64 = 64 * 1024;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionState {
    pub schema: u16,
    pub session_id: String,
    pub session_incarnation: String,
    pub holder_build_id: String,
    pub holder_pid: u32,
    pub process_state: RemoteProcessState,
    pub cols: u16,
    pub rows: u16,
    pub output_offset: u64,
    pub snapshot_sequence: u64,
    pub controller_epoch: u64,
    pub persistence: PersistenceCapability,
    pub created_at_unix_ms: u64,
}

impl SessionState {
    pub fn new(request: &LaunchRequest, incarnation: String, process_pid: u32) -> Self {
        Self {
            schema: STATE_SCHEMA,
            session_id: request.session_id.clone(),
            session_incarnation: incarnation,
            holder_build_id: BUILD_ID.to_string(),
            holder_pid: std::process::id(),
            process_state: RemoteProcessState::Running { pid: process_pid },
            cols: request.cols,
            rows: request.rows,
            output_offset: 0,
            snapshot_sequence: 0,
            controller_epoch: 0,
            persistence: request.persistence,
            created_at_unix_ms: unix_millis(),
        }
    }

    #[must_use]
    pub fn inspection(&self) -> SessionInspection {
        SessionInspection {
            session_id: self.session_id.clone(),
            session_incarnation: self.session_incarnation.clone(),
            holder_build_id: self.holder_build_id.clone(),
            holder_pid: self.holder_pid,
            process_state: self.process_state.clone(),
            cols: self.cols,
            rows: self.rows,
            output_offset: self.output_offset,
            snapshot_sequence: self.snapshot_sequence,
            controller_epoch: self.controller_epoch,
            persistence: self.persistence,
        }
    }
}

pub fn acquire_lock(path: &Path) -> io::Result<File> {
    let file = open_private_file(path)?;
    // SAFETY: `flock` operates only on the valid descriptor owned by `file`.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result < 0 {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "a Holder already owns this session",
        ));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

pub fn holder_lock_held(path: &Path) -> io::Result<bool> {
    let file = open_private_file(path)?;
    // SAFETY: `flock` operates only on the descriptor owned by `file`.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        // SAFETY: this process acquired the advisory lock above and releases
        // that same lock before the file is dropped.
        unsafe {
            libc::flock(file.as_raw_fd(), libc::LOCK_UN);
        }
        return Ok(false);
    }
    let error = io::Error::last_os_error();
    let code = error.raw_os_error();
    if code == Some(libc::EWOULDBLOCK) || code == Some(libc::EAGAIN) {
        Ok(true)
    } else {
        Err(error)
    }
}

pub fn write_state(path: &Path, state: &SessionState) -> io::Result<()> {
    reject_symlink(path)?;
    let bytes = serde_json::to_vec(state)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "state path has no parent"))?;
    let temporary = parent.join(format!(".session-{}.tmp", random_hex(12)?));
    let mut file = create_private_file(&temporary)?;
    let result = (|| {
        file.write_all(&bytes)?;
        file.sync_all()?;
        reject_symlink(path)?;
        fs::rename(&temporary, path)?;
        File::open(parent)?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

pub fn read_state(path: &Path) -> io::Result<SessionState> {
    reject_symlink(path)?;
    let mut file = File::open(path)?;
    if file.metadata()?.len() > MAX_STATE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "session state exceeds its size limit",
        ));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let state: SessionState = serde_json::from_slice(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if state.schema != STATE_SCHEMA {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported session state schema",
        ));
    }
    Ok(state)
}

pub fn initialize_auth(paths: &SessionPaths, token: &SessionToken) -> io::Result<()> {
    let expected = token_hash(token);
    match fs::read_to_string(&paths.auth) {
        Ok(existing) if constant_time_equal(existing.trim().as_bytes(), expected.as_bytes()) => {
            Ok(())
        }
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "session token does not match the existing Holder",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let mut file = create_private_file(&paths.auth)?;
            file.write_all(expected.as_bytes())?;
            file.write_all(b"\n")?;
            file.sync_all()
        }
        Err(error) => Err(error),
    }
}

pub fn authenticate(paths: &SessionPaths, token: &SessionToken) -> io::Result<bool> {
    reject_symlink(&paths.auth)?;
    let expected = fs::read_to_string(&paths.auth)?;
    Ok(constant_time_equal(
        expected.trim().as_bytes(),
        token_hash(token).as_bytes(),
    ))
}

pub fn remove_stale_socket(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => fs::remove_file(path),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "refusing to replace a non-socket Holder path",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[must_use]
pub fn process_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: signal zero performs an existence/permission check only.
    let result = unsafe { libc::kill(pid, 0) };
    result == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

pub fn random_hex(bytes: usize) -> io::Result<String> {
    let mut random = vec![0_u8; bytes];
    getrandom::fill(&mut random)
        .map_err(|error| io::Error::other(format!("secure random source failed: {error}")))?;
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes * 2);
    for byte in random {
        encoded.push(DIGITS[usize::from(byte >> 4)] as char);
        encoded.push(DIGITS[usize::from(byte & 0x0f)] as char);
    }
    Ok(encoded)
}

fn token_hash(token: &SessionToken) -> String {
    let digest = Sha256::digest(token.expose_secret().as_bytes());
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    for index in 0..left.len().max(right.len()) {
        let left = left.get(index).copied().unwrap_or(0);
        let right = right.get(index).copied().unwrap_or(0);
        difference |= usize::from(left ^ right);
    }
    difference == 0
}

fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::StatePaths;

    fn token(value: &str) -> SessionToken {
        SessionToken::new(value).expect("token")
    }

    #[test]
    fn auth_is_hashed_and_compared_without_persisting_the_bearer() {
        let temporary = tempfile::tempdir().expect("temp");
        let paths = StatePaths {
            root: temporary.path().join("state"),
            sessions: temporary.path().join("state/sessions"),
            launch_locks: temporary.path().join("state/launch-locks"),
        };
        crate::paths::ensure_private_dir(&temporary.path().join("state")).expect("root");
        crate::paths::ensure_private_dir(&paths.sessions).expect("sessions");
        crate::paths::ensure_private_dir(&paths.launch_locks).expect("launch locks");
        let session = paths.session("s1").expect("session");
        session.ensure().expect("dir");
        let bearer = token("0123456789abcdef");
        initialize_auth(&session, &bearer).expect("auth");
        let stored = fs::read_to_string(&session.auth).expect("read");
        assert!(!stored.contains(bearer.expose_secret()));
        assert!(authenticate(&session, &bearer).expect("match"));
        assert!(!authenticate(&session, &token("fedcba9876543210")).expect("mismatch"));
    }
}
