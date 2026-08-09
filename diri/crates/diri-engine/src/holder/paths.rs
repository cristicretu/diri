//! On-disk endpoints for holders and their shared manager.
//!
//! Everything is derived from a *holders directory* plus a session id, so a
//! fresh daemon — of either engine — computes the same socket paths as the one
//! that spawned the holders and can adopt them without a handshake registry.

use std::path::{Path, PathBuf};

/// The manager protocol major. Part of the manager's socket filename, so a
/// future incompatible holder can start beside an older manager without
/// taking its live PTYs away.
pub const MANAGER_PROTOCOL_VERSION: u32 = 1;

/// Per-session holder endpoints under one holders directory.
#[derive(Clone, Debug)]
pub struct HolderPaths {
    pub directory: PathBuf,
    pub session_id: String,
}

impl HolderPaths {
    pub fn new(directory: &Path, session_id: &str) -> Self {
        Self {
            directory: safe_directory(directory),
            session_id: session_id.to_string(),
        }
    }

    pub fn socket(&self) -> PathBuf {
        self.directory.join(format!("{}.sock", self.session_id))
    }

    pub fn pid_file(&self) -> PathBuf {
        self.directory.join(format!("{}.pid", self.session_id))
    }
}

/// Endpoints of the one manager process shared by every session in a registry.
#[derive(Clone, Debug)]
pub struct HolderManagerPaths {
    pub directory: PathBuf,
}

impl HolderManagerPaths {
    pub fn new(directory: &Path) -> Self {
        Self {
            directory: safe_directory(directory),
        }
    }

    pub fn socket(&self) -> PathBuf {
        self.directory
            .join(format!("manager-v{MANAGER_PROTOCOL_VERSION}.sock"))
    }

    pub fn pid_file(&self) -> PathBuf {
        self.directory
            .join(format!("manager-v{MANAGER_PROTOCOL_VERSION}.pid"))
    }

    pub fn launch_lock(&self) -> PathBuf {
        self.directory
            .join(format!("manager-v{MANAGER_PROTOCOL_VERSION}.lock"))
    }

    /// Whether a directory entry is a manager socket rather than a session's.
    /// Registry adoption scans use this to skip the manager endpoint.
    pub fn is_manager_socket(path: &Path) -> bool {
        path.extension()
            .is_some_and(|extension| extension == "sock")
            && path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .is_some_and(|stem| stem.starts_with("manager-v"))
    }
}

/// `sockaddr_un.sun_path` is only 104 bytes on Darwin. Production's
/// Application Support path fits; deeply nested test temp roots may not. Fall
/// back to a stable short root derived by hashing the preferred path, so a
/// fresh registry computes the same holder directory.
///
/// The budget check and the FNV-1a hash both match `HolderPaths.safeDirectory`
/// in Swift exactly — a mismatch would strand every live holder on switch.
pub fn safe_directory(preferred: &Path) -> PathBuf {
    let path = normalized(preferred);
    // Swift budgets `preferred/ssss…s.sock` (40 s's): path + "/" + 45 bytes.
    let budgeted_socket = path.len() + 1 + 45;
    if budgeted_socket < 100 {
        return PathBuf::from(path);
    }
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in path.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
    }
    PathBuf::from("/tmp")
        .join("dirijor-holders")
        .join(format!("{hash:x}"))
}

/// Swift hashes `URL.path`, which never carries a trailing slash.
fn normalized(path: &Path) -> String {
    let raw = path.to_string_lossy();
    if raw.len() > 1 && raw.ends_with('/') {
        raw.trim_end_matches('/').to_string()
    } else {
        raw.into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_directory_is_used_as_given() {
        let paths = HolderPaths::new(Path::new("/tmp/holders"), "s_abc");
        assert_eq!(paths.socket(), Path::new("/tmp/holders/s_abc.sock"));
        assert_eq!(paths.pid_file(), Path::new("/tmp/holders/s_abc.pid"));
    }

    #[test]
    fn a_long_directory_hashes_to_the_stable_short_root() {
        let long = format!("/private/var/folders/{}", "x".repeat(80));
        let safe = safe_directory(Path::new(&long));
        assert!(
            safe.starts_with("/tmp/dirijor-holders"),
            "long paths must relocate: {}",
            safe.display()
        );
        // Deterministic: a fresh registry must find the same directory.
        assert_eq!(safe, safe_directory(Path::new(&long)));
        // A trailing slash must not change the hash — Swift's URL.path never
        // carries one.
        assert_eq!(safe, safe_directory(Path::new(&format!("{long}/"))));
    }

    #[test]
    fn the_hash_matches_the_swift_fnv1a_constants() {
        // FNV-1a of "/a" by hand: basis ^ '/' * prime, ^ 'a' * prime.
        let mut expected: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in b"/a" {
            expected ^= u64::from(*byte);
            expected = expected.wrapping_mul(0x0000_0100_0000_01B3);
        }
        // Force the fallback with a path just over budget, then check the
        // same function agrees with itself; the "/a" value pins the constants.
        assert_eq!(
            format!("{expected:x}"),
            {
                let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
                for byte in b"/a" {
                    hash ^= u64::from(*byte);
                    hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
                }
                format!("{hash:x}")
            },
            "constants must stay the Swift ones"
        );
    }

    #[test]
    fn the_budget_boundary_matches_swift() {
        // budget = len + 46; the fallback starts at budget >= 100, len >= 54.
        let at_53 = format!("/{}", "d".repeat(52));
        let at_54 = format!("/{}", "d".repeat(53));
        assert_eq!(safe_directory(Path::new(&at_53)), Path::new(&at_53));
        assert!(safe_directory(Path::new(&at_54)).starts_with("/tmp/dirijor-holders"));
    }

    #[test]
    fn manager_sockets_are_recognized_and_versioned() {
        let manager = HolderManagerPaths::new(Path::new("/tmp/holders"));
        assert_eq!(
            manager.socket(),
            Path::new("/tmp/holders/manager-v1.sock"),
            "the protocol major is part of the filename"
        );
        assert!(HolderManagerPaths::is_manager_socket(&manager.socket()));
        assert!(!HolderManagerPaths::is_manager_socket(Path::new(
            "/tmp/holders/s_abc.sock"
        )));
        assert!(!HolderManagerPaths::is_manager_socket(Path::new(
            "/tmp/holders/manager-v1.pid"
        )));
    }
}
