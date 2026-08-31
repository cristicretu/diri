//! Minimal durable facts shared by the Engine and hook CLI.
//!
//! Each local session owns a directory containing two independently written
//! files:
//!
//! - `recovery.json` is an Engine snapshot with only the identity required to
//!   rediscover a live Holder when the global Registry is unavailable.
//! - `last-activity.json` is written by the hook/notify CLI before it attempts
//!   daemon delivery, so a daemon outage cannot erase the latest lifecycle
//!   signal.
//!
//! The files are deliberately not a second SessionRecord. UI state, project
//! ordering, pins, prompts, and other mutable product metadata remain owned by
//! the Registry.

use std::fs::{DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::{DateMillis, SessionId};

pub const RECOVERY_CAPSULE_FILE: &str = "recovery.json";
pub const LAST_ACTIVITY_FILE: &str = "last-activity.json";

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRecoveryCapsule {
    pub version: u32,
    pub session_id: SessionId,
    pub manifest_id: String,
    pub cwd: String,
    pub created_at: DateMillis,
    #[serde(
        rename = "agentSessionID",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub agent_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_path: Option<String>,
}

impl SessionRecoveryCapsule {
    pub const VERSION: u32 = 1;
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HookActivitySeed {
    pub version: u32,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<String>,
    pub occurred_at_ms: u64,
    #[serde(
        rename = "agentSessionID",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub agent_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notification_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
}

impl HookActivitySeed {
    pub const VERSION: u32 = 1;
}

/// Filesystem adapter for one session's recovery directory.
#[derive(Clone, Debug)]
pub struct SessionRecoveryStore {
    directory: PathBuf,
}

impl SessionRecoveryStore {
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
        }
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn write_capsule(&self, capsule: &SessionRecoveryCapsule) -> io::Result<()> {
        write_json_atomic(&self.directory, RECOVERY_CAPSULE_FILE, capsule)
    }

    pub fn read_capsule(&self) -> io::Result<Option<SessionRecoveryCapsule>> {
        read_json(&self.directory.join(RECOVERY_CAPSULE_FILE))
    }

    pub fn write_activity(&self, seed: &HookActivitySeed) -> io::Result<()> {
        write_json_atomic(&self.directory, LAST_ACTIVITY_FILE, seed)
    }

    pub fn read_activity(&self) -> io::Result<Option<HookActivitySeed>> {
        read_json(&self.directory.join(LAST_ACTIVITY_FILE))
    }

    /// Removes only the two files this module owns, then the directory if it
    /// became empty. Provider-specific durable storage may live beside them.
    pub fn remove_owned_files(&self) -> io::Result<()> {
        for name in [RECOVERY_CAPSULE_FILE, LAST_ACTIVITY_FILE] {
            match std::fs::remove_file(self.directory.join(name)) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        match std::fs::remove_dir(&self.directory) {
            Ok(()) => Ok(()),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty
                ) =>
            {
                Ok(())
            }
            Err(error) => Err(error),
        }
    }
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> io::Result<Option<T>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn write_json_atomic<T: Serialize>(directory: &Path, name: &str, value: &T) -> io::Result<()> {
    create_private_directory(directory)?;
    static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);
    let temporary = directory.join(format!(
        ".{name}.tmp-{}-{}",
        std::process::id(),
        NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed)
    ));
    let body = serde_json::to_vec(value)?;
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    if let Err(error) = file.write_all(&body).and_then(|()| file.sync_all()) {
        drop(file);
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    drop(file);
    if let Err(error) = std::fs::rename(&temporary, directory.join(name)) {
        let _ = std::fs::remove_file(temporary);
        return Err(error);
    }
    Ok(())
}

fn create_private_directory(path: &Path) -> io::Result<()> {
    let mut builder = DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capsule() -> SessionRecoveryCapsule {
        SessionRecoveryCapsule {
            version: SessionRecoveryCapsule::VERSION,
            session_id: SessionId::new("s_one"),
            manifest_id: "claude-code".into(),
            cwd: "/tmp/project".into(),
            created_at: DateMillis(42.0),
            agent_session_id: Some("conversation".into()),
            transcript_path: None,
        }
    }

    #[test]
    fn independently_owned_files_survive_each_others_updates() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let store = SessionRecoveryStore::new(directory.path().join("s_one"));
        let seed = HookActivitySeed {
            version: HookActivitySeed::VERSION,
            kind: "claude-hook".into(),
            event: Some("Stop".into()),
            occurred_at_ms: 99,
            agent_session_id: Some("conversation".into()),
            transcript_path: None,
            notification_type: None,
            tool_name: None,
        };

        store.write_capsule(&capsule()).expect("capsule");
        store.write_activity(&seed).expect("activity");
        let mut updated = capsule();
        updated.transcript_path = Some("/tmp/transcript.jsonl".into());
        store.write_capsule(&updated).expect("capsule update");

        assert_eq!(store.read_capsule().expect("read"), Some(updated));
        assert_eq!(store.read_activity().expect("read"), Some(seed));
    }

    #[test]
    fn cleanup_does_not_remove_provider_storage() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let session = directory.path().join("s_one");
        let store = SessionRecoveryStore::new(&session);
        store.write_capsule(&capsule()).expect("capsule");
        std::fs::create_dir_all(session.join("provider")).expect("provider directory");

        store.remove_owned_files().expect("cleanup");

        assert!(session.join("provider").is_dir());
        assert!(!session.join(RECOVERY_CAPSULE_FILE).exists());
    }
}
