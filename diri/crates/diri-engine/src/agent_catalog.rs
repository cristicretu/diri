//! Per-execution-target Agent discovery preferences and short-lived snapshots.
//!
//! Configuration is Engine-owned because every spawn and resume path must use
//! the same executable decision. The desktop only renders these facts.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{self, Write as _};
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
        let config = match diri_platform::security::read_owned(&path, true).and_then(|mut file| {
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut file, &mut bytes)?;
            Ok(bytes)
        }) {
            Ok(bytes) => {
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
        diri_platform::security::private_dir_all(parent)?;
        let nonce = format!("{}-{:?}", std::process::id(), std::thread::current().id());
        let temporary = parent.join(format!(".agents-{nonce}.tmp"));
        let mut file = diri_platform::security::create_private(&temporary)?;
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
    let path = crate::local_path::search_path(None, std::env::vars());
    let detected_path = resolve_on_path(binary, &path);
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

pub(crate) fn resolve_on_path(binary: &str, path: &str) -> Option<String> {
    let env = vec![("PATH".into(), path.into())];
    let cwd = std::env::current_dir().ok()?;
    diri_platform::launch::find_executable(binary, &env, &cwd)
        .ok()
        .and_then(|p| executable_path(&p))
}

fn executable_path(path: &Path) -> Option<String> {
    diri_platform::launch::is_executable(path).then(|| path.to_string_lossy().into_owned())
}

fn expand_home(path: &str) -> io::Result<PathBuf> {
    if path == "~" || path.starts_with("~/") {
        let home = diri_platform::home_dir()
            .map(|p| p.into_os_string())
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
    if !(path.starts_with('/')
        || Path::new(path).is_absolute()
        || path == "~"
        || path.starts_with("~/"))
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
    #[cfg(unix)]
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[cfg(unix)]
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
