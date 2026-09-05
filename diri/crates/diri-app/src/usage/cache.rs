use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use super::model::UsageHourAgg;

pub(crate) const CACHE_VERSION: u32 = 3;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct UsageFileEntry {
    pub size: u64,
    pub offset: u64,
    pub modified_ns: u64,
    pub device: Option<u64>,
    pub inode: Option<u64>,
    pub tail_hash: u64,
    pub hours: BTreeMap<i64, UsageHourAgg>,
    pub details: super::dashboard::ModelHours,
    pub model: Option<String>,
}

impl UsageFileEntry {
    pub fn empty(size: u64, modified_ns: u64, device: Option<u64>, inode: Option<u64>) -> Self {
        Self {
            size,
            offset: 0,
            modified_ns,
            device,
            inode,
            tail_hash: 0,
            hours: BTreeMap::new(),
            details: BTreeMap::new(),
            model: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct UsageCacheFile {
    pub version: u32,
    pub files: BTreeMap<String, UsageFileEntry>,
    pub seen: BTreeMap<i64, Vec<u64>>,
}

impl Default for UsageCacheFile {
    fn default() -> Self {
        Self {
            version: CACHE_VERSION,
            files: BTreeMap::new(),
            seen: BTreeMap::new(),
        }
    }
}

pub(crate) fn load(path: &Path) -> UsageCacheFile {
    let Ok(data) = fs::read(path) else {
        return UsageCacheFile::default();
    };
    let Ok(cache) = serde_json::from_slice::<UsageCacheFile>(&data) else {
        return UsageCacheFile::default();
    };
    if cache.version == CACHE_VERSION {
        cache
    } else {
        UsageCacheFile::default()
    }
}

pub(crate) fn save(path: &Path, cache: &UsageCacheFile) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let data = serde_json::to_vec(cache).map_err(io::Error::other)?;
    let temporary = temporary_path(path);
    fs::write(&temporary, data)?;
    fs::rename(temporary, path)
}

fn temporary_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map_or_else(|| "usage-cache.json".into(), |name| name.to_os_string());
    name.push(".tmp");
    path.with_file_name(name)
}
