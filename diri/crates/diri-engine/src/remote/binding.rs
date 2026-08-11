//! Owner-only local authentication bindings for remote Holders.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use diri_proto::remote_pty::{ProtocolVersion, SessionToken};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteBinding {
    pub session_id: String,
    pub host_id: String,
    pub helper_build_id: String,
    pub protocol: ProtocolVersion,
    pub session_token: SessionToken,
    pub session_incarnation: String,
    #[serde(default)]
    pub last_output_offset: u64,
}

#[derive(Clone, Debug)]
pub struct RemoteBindingStore {
    root: PathBuf,
}

impl RemoteBindingStore {
    pub fn new(root: impl Into<PathBuf>) -> io::Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
        Ok(Self { root })
    }

    pub fn save(&self, binding: &RemoteBinding) -> io::Result<()> {
        validate_identifier(&binding.session_id)?;
        let path = self.path(&binding.session_id);
        reject_symlink(&path)?;
        let nonce = random_hex()?;
        let temporary = self.root.join(format!(".tmp-{nonce}"));
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)?;
        let result = (|| {
            serde_json::to_writer(&mut file, binding)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            fs::rename(&temporary, &path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    /// Every readable, well-formed, owner-only binding in the store.
    ///
    /// One unreadable file is not allowed to strand the others. These describe
    /// independent live sessions, so rejecting the whole set over a single
    /// truncated write or a stray mode would orphan every remote Holder the
    /// Engine could otherwise have re-adopted. Per-file checks are unchanged
    /// and still refuse anything not owner-only, oversized, symlinked or
    /// malformed — the offender is skipped and named instead of poisoning the
    /// batch. Only failing to read the directory itself is fatal.
    pub fn load_all(&self) -> io::Result<Vec<RemoteBinding>> {
        let mut bindings = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            match Self::load_one(&path) {
                Ok(binding) => bindings.push(binding),
                Err(error) => eprintln!(
                    "diri-engine: skipping unusable remote binding {}: {error}",
                    path.display()
                ),
            }
        }
        Ok(bindings)
    }

    fn load_one(path: &Path) -> io::Result<RemoteBinding> {
        reject_symlink(path)?;
        let metadata = fs::metadata(path)?;
        if !metadata.is_file() || metadata.permissions().mode() & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "remote binding file is not owner-only",
            ));
        }
        if metadata.len() > 64 * 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "remote binding file exceeds 64 KiB",
            ));
        }
        let binding: RemoteBinding = serde_json::from_slice(&fs::read(path)?)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        validate_identifier(&binding.session_id)?;
        Ok(binding)
    }

    pub fn update_output_offset(&self, session_id: &str, offset: u64) -> io::Result<()> {
        validate_identifier(session_id)?;
        let path = self.path(session_id);
        reject_symlink(&path)?;
        let metadata = fs::metadata(&path)?;
        if !metadata.is_file()
            || metadata.permissions().mode() & 0o077 != 0
            || metadata.len() > 64 * 1024
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "remote binding metadata is invalid",
            ));
        }
        let mut binding: RemoteBinding = serde_json::from_slice(&fs::read(path)?)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if binding.session_id != session_id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "remote binding identity does not match its filename",
            ));
        }
        if offset <= binding.last_output_offset {
            return Ok(());
        }
        binding.last_output_offset = offset;
        self.save(&binding)
    }

    pub fn remove(&self, session_id: &str) -> io::Result<()> {
        validate_identifier(session_id)?;
        let path = self.path(session_id);
        reject_symlink(&path)?;
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn path(&self, session_id: &str) -> PathBuf {
        self.root.join(format!("{session_id}.json"))
    }
}

fn validate_identifier(value: &str) -> io::Result<()> {
    let valid = !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    if valid {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "remote binding identifier is invalid",
        ))
    }
}

fn reject_symlink(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "remote binding path is a symlink",
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn random_hex() -> io::Result<String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|error| io::Error::other(format!("secure random source failed: {error}")))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bindings describe independent live sessions. Rejecting the whole set
    /// over one truncated write would orphan every remote Holder the Engine
    /// could still have re-adopted.
    #[test]
    fn one_unusable_binding_does_not_strand_the_others() {
        let temporary = tempfile::tempdir().expect("temp");
        let store = RemoteBindingStore::new(temporary.path().join("bindings")).expect("store");
        let good = RemoteBinding {
            session_id: "session-good".into(),
            host_id: "host-1".into(),
            helper_build_id: "build-1".into(),
            protocol: ProtocolVersion::CURRENT,
            session_token: SessionToken::new("0123456789abcdef").expect("token"),
            session_incarnation: "incarnation-1".into(),
            last_output_offset: 0,
        };
        store.save(&good).expect("save");
        std::fs::write(
            temporary
                .path()
                .join("bindings")
                .join("session-broken.json"),
            b"{ this is not json",
        )
        .expect("write malformed binding");

        let loaded = store
            .load_all()
            .expect("a malformed file must not fail the batch");
        assert_eq!(loaded.len(), 1, "the readable binding must survive");
        assert_eq!(loaded[0].session_id, "session-good");
    }

    #[test]
    fn binding_is_owner_only_and_redacts_the_bearer_from_debug() {
        let temporary = tempfile::tempdir().expect("temp");
        let store = RemoteBindingStore::new(temporary.path().join("bindings")).expect("store");
        let binding = RemoteBinding {
            session_id: "session-1".into(),
            host_id: "host-1".into(),
            helper_build_id: "build-1".into(),
            protocol: ProtocolVersion::CURRENT,
            session_token: SessionToken::new("0123456789abcdef").expect("token"),
            session_incarnation: "incarnation-1".into(),
            last_output_offset: 0,
        };
        store.save(&binding).expect("save");
        let loaded = store.load_all().expect("load");
        assert_eq!(loaded.len(), 1);
        assert!(!format!("{:?}", loaded[0]).contains("0123456789abcdef"));
        let mode = fs::metadata(temporary.path().join("bindings/session-1.json"))
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        store.update_output_offset("session-1", 42).expect("offset");
        assert_eq!(store.load_all().expect("reload")[0].last_output_offset, 42);
    }
}
