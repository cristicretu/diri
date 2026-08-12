//! Per-execution-target Agent discovery preferences and short-lived snapshots.
//!
//! Configuration is Engine-owned because every spawn and resume path must use
//! the same executable decision. The desktop only renders these facts.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use diri_proto::AgentReadinessResult;
use serde::{Deserialize, Serialize};

pub const CACHE_TTL: Duration = Duration::from_secs(5 * 60);
const CONFIG_VERSION: u32 = 1;
const LOCAL_TARGET: &str = "local";

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConfigFile {
    #[serde(default = "config_version")]
    version: u32,
    #[serde(default)]
    targets: BTreeMap<String, BTreeMap<String, AgentPreference>>,
}

const fn config_version() -> u32 {
    CONFIG_VERSION
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentPreference {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_in_quick_create: Option<bool>,
}

#[derive(Clone)]
struct CachedCatalog {
    inserted: Instant,
    result: AgentReadinessResult,
}

pub struct AgentCatalogStore {
    path: PathBuf,
    config: ConfigFile,
    cache: HashMap<String, CachedCatalog>,
}

impl AgentCatalogStore {
    pub fn new(path: impl Into<PathBuf>) -> io::Result<Self> {
        let path = path.into();
        let config = match fs::read(&path) {
            Ok(bytes) => {
                let metadata = fs::symlink_metadata(&path)?;
                if metadata.file_type().is_symlink()
                    || !metadata.is_file()
                    || metadata.permissions().mode() & 0o077 != 0
                {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "Agent configuration must be an owner-only regular file",
                    ));
                }
                let decoded: ConfigFile = serde_json::from_slice(&bytes)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                if decoded.version > CONFIG_VERSION {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Agent configuration was written by a newer Diri build",
                    ));
                }
                decoded
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => ConfigFile::default(),
            Err(error) => return Err(error),
        };
        Ok(Self {
            path,
            config,
            cache: HashMap::new(),
        })
    }

    #[must_use]
    pub fn empty(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            config: ConfigFile::default(),
            cache: HashMap::new(),
        }
    }

    #[must_use]
    pub fn preference(&self, host: Option<&str>, agent: &str) -> AgentPreference {
        self.config
            .targets
            .get(&target_key(host))
            .and_then(|agents| agents.get(agent))
            .cloned()
            .unwrap_or_default()
    }

    pub fn configure(
        &mut self,
        host: Option<&str>,
        agent: &str,
        preference: AgentPreference,
    ) -> io::Result<()> {
        validate_component("agent id", agent)?;
        if let Some(host) = host {
            validate_component("host id", host)?;
        }
        if let Some(path) = preference.executable_path.as_deref() {
            validate_user_path(path)?;
        }
        let key = target_key(host);
        let previous = self.config.clone();
        self.config
            .targets
            .entry(key.clone())
            .or_default()
            .insert(agent.to_owned(), preference);
        if let Err(error) = self.save() {
            self.config = previous;
            return Err(error);
        }
        self.cache.remove(&key);
        Ok(())
    }

    #[must_use]
    pub fn cached(&self, host: Option<&str>) -> Option<AgentReadinessResult> {
        self.cache
            .get(&target_key(host))
            .filter(|cached| cached.inserted.elapsed() <= CACHE_TTL)
            .map(|cached| cached.result.clone())
    }

    pub fn cache(&mut self, host: Option<&str>, result: AgentReadinessResult) {
        self.cache.insert(
            target_key(host),
            CachedCatalog {
                inserted: Instant::now(),
                result,
            },
        );
    }

    pub fn invalidate(&mut self, host: Option<&str>) {
        self.cache.remove(&target_key(host));
    }

    fn save(&self) -> io::Result<()> {
        let parent = self.path.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Agent configuration path has no parent",
            )
        })?;
        fs::create_dir_all(parent)?;
        let nonce = format!("{}-{:?}", std::process::id(), std::thread::current().id());
        let temporary = parent.join(format!(".agents-{nonce}.tmp"));
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)?;
        let result = (|| {
            serde_json::to_writer_pretty(&mut file, &self.config)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            fs::rename(&temporary, &self.path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}

#[must_use]
pub fn resolve_local(binary: &str, configured: Option<&str>) -> ExecutableResolution {
    let detected_path = resolve_on_path(binary, &std::env::var("PATH").unwrap_or_default());
    let (configured_path, configured_error) = match configured {
        Some(path) => match validate_executable(path) {
            Ok(path) => (Some(path), None),
            Err(error) => (None, Some(error.to_string())),
        },
        None => (None, None),
    };
    ExecutableResolution {
        detected_path,
        configured_path,
        configured_error,
    }
}

#[derive(Clone, Debug)]
pub struct ExecutableResolution {
    pub detected_path: Option<String>,
    pub configured_path: Option<String>,
    pub configured_error: Option<String>,
}

pub fn validate_executable(path: &str) -> io::Result<String> {
    validate_user_path(path)?;
    let expanded = expand_home(path)?;
    if !expanded.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "executable path must be absolute or home-relative",
        ));
    }
    executable_path(&expanded).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "path is not an executable regular file",
        )
    })
}

fn resolve_on_path(binary: &str, path: &str) -> Option<String> {
    if binary.contains('/') {
        return validate_executable(binary).ok();
    }
    path.split(':')
        .filter(|directory| !directory.is_empty())
        .map(|directory| Path::new(directory).join(binary))
        .find_map(|candidate| executable_path(&candidate))
}

fn executable_path(path: &Path) -> Option<String> {
    let metadata = fs::metadata(path).ok()?;
    (metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .then(|| path.to_string_lossy().into_owned())
}

fn expand_home(path: &str) -> io::Result<PathBuf> {
    if path == "~" || path.starts_with("~/") {
        let home = std::env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))?;
        let mut expanded = PathBuf::from(home);
        if let Some(rest) = path.strip_prefix("~/") {
            expanded.push(rest);
        }
        Ok(expanded)
    } else {
        Ok(PathBuf::from(path))
    }
}

fn validate_user_path(path: &str) -> io::Result<()> {
    if path.is_empty() || path.len() > 4_096 || path.as_bytes().contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "executable path must be non-empty, NUL-free, and at most 4096 bytes",
        ));
    }
    if !(Path::new(path).is_absolute() || path == "~" || path.starts_with("~/"))
        || path
            .split('/')
            .any(|component| matches!(component, "." | ".."))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "executable path must be normalized and absolute or home-relative",
        ));
    }
    Ok(())
}

fn validate_component(label: &str, value: &str) -> io::Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{label} contains unsupported characters"),
        ));
    }
    Ok(())
}

fn target_key(host: Option<&str>) -> String {
    host.unwrap_or(LOCAL_TARGET).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_only_config_round_trips_and_invalidates_cache() {
        let temp = tempfile::tempdir().expect("temp");
        let path = temp.path().join("agents.json");
        let mut store = AgentCatalogStore::new(&path).expect("store");
        store.cache(
            None,
            AgentReadinessResult {
                host: None,
                scanned_at: None,
                agents: Vec::new(),
            },
        );
        store
            .configure(
                None,
                "codex",
                AgentPreference {
                    executable_path: Some("/usr/bin/true".into()),
                    show_in_quick_create: Some(false),
                },
            )
            .expect("configure");
        assert!(store.cached(None).is_none());
        assert_eq!(
            fs::metadata(&path).expect("metadata").permissions().mode() & 0o777,
            0o600
        );
        let loaded = AgentCatalogStore::new(path).expect("reload");
        assert_eq!(
            loaded.preference(None, "codex").executable_path.as_deref(),
            Some("/usr/bin/true")
        );
    }
}
