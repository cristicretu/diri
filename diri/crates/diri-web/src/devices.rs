//! Devices paired with a tailnet-admitted gateway.
//!
//! WhoIs says *whose* device is calling; pairing says the owner meant to let
//! *this* one in. A device is recorded by Tailscale's stable node id after it
//! presents a one-time pairing code, and revoking it is deleting that record.
//! The list is a small owner-only JSON file so the Mac app, the standalone
//! `diri-web` and its `revoke` subcommand all see one truth; a running
//! gateway notices another process's edit by the file's stamp.
//!
//! A pairing code is a single-use 128-bit secret that expires after
//! [`CODE_LIFETIME`]. It sits in its own owner-only file only until it is
//! redeemed, replaced or expired.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::auth::{constant_time_eq, mint_secret, restrict, write_owner_only};
use crate::tailnet::Identity;

pub const CODE_LIFETIME: Duration = Duration::from_secs(10 * 60);
const CODE_BYTES: usize = 16;
const FORMAT_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Device {
    /// Tailscale's stable node id.
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub os: String,
    #[serde(default)]
    pub login: String,
    /// Seconds since the Unix epoch.
    pub paired_at: u64,
}

#[derive(Default, Serialize, Deserialize)]
struct DeviceFile {
    version: u32,
    devices: Vec<Device>,
}

#[derive(Default)]
struct Loaded {
    stamp: Option<(SystemTime, u64)>,
    devices: Vec<Device>,
}

pub struct Devices {
    path: PathBuf,
    loaded: Mutex<Loaded>,
    generation: watch::Sender<u64>,
}

impl Devices {
    /// The store at `path`. Nothing is created until the first pairing.
    #[must_use]
    pub fn open(path: PathBuf) -> Self {
        Self {
            path,
            loaded: Mutex::new(Loaded::default()),
            generation: watch::channel(0).0,
        }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn code_path(&self) -> PathBuf {
        let mut path = self.path.as_os_str().to_owned();
        path.push(".pairing");
        PathBuf::from(path)
    }

    /// The paired devices, oldest first. An unreadable or corrupt file reads
    /// as no devices: admission fails closed.
    #[must_use]
    pub fn list(&self) -> Vec<Device> {
        let stamp = std::fs::metadata(&self.path)
            .ok()
            .and_then(|metadata| Some((metadata.modified().ok()?, metadata.len())));
        let Ok(mut loaded) = self.loaded.lock() else {
            return Vec::new();
        };
        if loaded.stamp != stamp || stamp.is_none() {
            loaded.devices = stamp
                .and_then(|_| std::fs::read(&self.path).ok())
                .and_then(|bytes| serde_json::from_slice::<DeviceFile>(&bytes).ok())
                .filter(|file| file.version == FORMAT_VERSION)
                .map(|file| file.devices)
                .unwrap_or_default();
            loaded.stamp = stamp;
        }
        loaded.devices.clone()
    }

    #[must_use]
    pub fn is_paired(&self, node: &str) -> bool {
        self.list().iter().any(|device| device.id == node)
    }

    /// Bumped by every change this process makes; [`crate::tailnet::revoked`]
    /// waits on it.
    #[must_use]
    pub fn watch(&self) -> watch::Receiver<u64> {
        self.generation.subscribe()
    }

    /// Forgets one device. Its open connections close and its next request
    /// is refused with `device_not_paired`. Returns whether it was paired.
    pub fn revoke(&self, id: &str) -> io::Result<bool> {
        let mut devices = self.list();
        let before = devices.len();
        devices.retain(|device| device.id != id);
        if devices.len() == before {
            return Ok(false);
        }
        self.store(devices)?;
        Ok(true)
    }

    /// Forgets every device and any outstanding pairing code.
    pub fn revoke_all(&self) -> io::Result<()> {
        remove_if_present(&self.code_path())?;
        self.store(Vec::new())
    }

    /// Mints the one live pairing code, replacing any earlier one.
    pub fn mint_code(&self) -> io::Result<String> {
        let code = mint_secret(CODE_BYTES)?;
        let expires = unix_now().saturating_add(CODE_LIFETIME.as_secs());
        self.write_atomically(&self.code_path(), format!("{code} {expires}\n").as_bytes())?;
        Ok(code)
    }

    /// Pairs `identity` if `presented` is the live code. The code is spent
    /// whether or not the device was already paired.
    pub fn redeem(&self, presented: &str, identity: &Identity) -> io::Result<bool> {
        let code_path = self.code_path();
        let Ok(stored) = std::fs::read_to_string(&code_path) else {
            return Ok(false);
        };
        let mut parts = stored.split_whitespace();
        let (Some(code), Some(expires)) = (parts.next(), parts.next()) else {
            remove_if_present(&code_path)?;
            return Ok(false);
        };
        if expires
            .parse::<u64>()
            .map_or(true, |expires| unix_now() > expires)
        {
            remove_if_present(&code_path)?;
            return Ok(false);
        }
        if !constant_time_eq(presented, code) {
            return Ok(false);
        }
        remove_if_present(&code_path)?;
        let mut devices = self.list();
        devices.retain(|device| device.id != identity.node);
        devices.push(Device {
            id: identity.node.clone(),
            name: identity.name.clone(),
            os: identity.os.clone(),
            login: identity.login.clone(),
            paired_at: unix_now(),
        });
        self.store(devices)?;
        Ok(true)
    }

    fn store(&self, devices: Vec<Device>) -> io::Result<()> {
        let file = DeviceFile {
            version: FORMAT_VERSION,
            devices,
        };
        let bytes = serde_json::to_vec_pretty(&file).map_err(io::Error::other)?;
        self.write_atomically(&self.path, &bytes)?;
        if let Ok(mut loaded) = self.loaded.lock() {
            // Force a reload so the stamp matches what is on disk.
            loaded.stamp = None;
        }
        self.generation.send_modify(|generation| *generation += 1);
        Ok(())
    }

    fn write_atomically(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
            restrict(parent, 0o700)?;
        }
        let mut staging = path.as_os_str().to_owned();
        staging.push(format!(".{}.tmp", std::process::id()));
        let staging = PathBuf::from(staging);
        let written =
            write_owner_only(&staging, bytes).and_then(|()| std::fs::rename(&staging, path));
        if written.is_err() {
            let _ = std::fs::remove_file(&staging);
        }
        written
    }
}

fn remove_if_present(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn phone() -> Identity {
        Identity {
            node: "nPhone".into(),
            name: "phone".into(),
            os: "iOS".into(),
            login: "me@example.com".into(),
        }
    }

    #[test]
    fn a_code_pairs_one_device_once() {
        let dir = tempfile::tempdir().unwrap();
        let devices = Devices::open(dir.path().join("nested").join("devices.json"));
        assert!(!devices.is_paired("nPhone"));
        let code = devices.mint_code().unwrap();
        assert!(!devices.redeem("wrong", &phone()).unwrap());
        assert!(devices.redeem(&code, &phone()).unwrap());
        assert!(devices.is_paired("nPhone"));
        // Spent: a second device cannot reuse it.
        let mut laptop = phone();
        laptop.node = "nLaptop".into();
        assert!(!devices.redeem(&code, &laptop).unwrap());
        assert!(!devices.is_paired("nLaptop"));
        // Minting again replaces, never adds, a code.
        let first = devices.mint_code().unwrap();
        let second = devices.mint_code().unwrap();
        assert!(!devices.redeem(&first, &laptop).unwrap());
        assert!(devices.redeem(&second, &laptop).unwrap());
        assert_eq!(devices.list().len(), 2);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(devices.path()), 0o600);
            assert_eq!(mode(devices.path().parent().unwrap()), 0o700);
        }
    }

    #[test]
    fn an_expired_code_is_refused_and_removed() {
        let dir = tempfile::tempdir().unwrap();
        let devices = Devices::open(dir.path().join("devices.json"));
        std::fs::write(devices.code_path(), "abc 1\n").unwrap();
        assert!(!devices.redeem("abc", &phone()).unwrap());
        assert!(!devices.code_path().exists());
    }

    #[test]
    fn revoking_is_seen_by_watchers_and_by_another_process() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("devices.json");
        let gateway = Devices::open(path.clone());
        let settings = Devices::open(path);
        let code = settings.mint_code().unwrap();
        assert!(gateway.redeem(&code, &phone()).unwrap());
        assert!(settings.is_paired("nPhone"));

        let watcher = gateway.watch();
        assert!(gateway.revoke("nPhone").unwrap());
        assert!(watcher.has_changed().unwrap());
        assert!(!settings.is_paired("nPhone"));
        assert!(!gateway.revoke("nPhone").unwrap());

        let code = settings.mint_code().unwrap();
        assert!(settings.redeem(&code, &phone()).unwrap());
        // Another process's edit is picked up from the file.
        assert!(gateway.is_paired("nPhone"));
        settings.revoke_all().unwrap();
        assert!(!gateway.is_paired("nPhone"));
    }

    #[test]
    fn a_corrupt_file_admits_nobody() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("devices.json");
        std::fs::write(&path, "{not json").unwrap();
        assert!(Devices::open(path).list().is_empty());
    }
}
