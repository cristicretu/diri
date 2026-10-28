//! Directories recently chosen in Insert Path and Go to Directory, most
//! recent first, kept per machine in one small JSON file beside the prefs.
//!
//! The file is read when a picker opens and rewritten when a choice is made,
//! both off the UI thread, so two windows never hold a stale copy for long and
//! nothing is kept in memory between pickers.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Directories remembered per machine.
pub const RECENT_LIMIT: usize = 30;
/// Machine key for this Mac. Insert Path only browses local files today; a
/// remote host would key by its host id.
pub const LOCAL_HOST: &str = "local";
const FILE_NAME: &str = "path-recents.json";

#[derive(Debug, Default, Deserialize, Serialize)]
struct RecentsFile {
    #[serde(default)]
    hosts: BTreeMap<String, Vec<String>>,
}

/// The recents file next to the prefs, or `None` in tests, which must never
/// touch the real home.
pub fn default_path() -> Option<PathBuf> {
    if cfg!(test) {
        return None;
    }
    crate::store::Prefs::path()
        .parent()
        .map(|parent| parent.join(FILE_NAME))
}

fn read(path: &Path) -> RecentsFile {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn write(path: &Path, file: &RecentsFile) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension(format!("json.{}.tmp", std::process::id()));
    fs::write(&temporary, serde_json::to_vec_pretty(file)?)?;
    fs::rename(&temporary, path)
}

/// `host`'s recent directories that still exist, most recent first.
/// Blocking; call off the UI thread.
pub fn load(path: &Path, host: &str) -> Vec<PathBuf> {
    read(path)
        .hosts
        .remove(host)
        .unwrap_or_default()
        .into_iter()
        .map(PathBuf::from)
        .filter(|directory| directory.is_absolute() && directory.is_dir())
        .collect()
}

/// Moves `directory` to the front of `host`'s list.
pub fn record(path: &Path, host: &str, directory: &Path) -> io::Result<()> {
    let Some(directory) = directory.to_str().filter(|_| directory.is_absolute()) else {
        return Ok(());
    };
    let directory = directory.trim_end_matches('/');
    let directory = if directory.is_empty() { "/" } else { directory };
    let mut file = read(path);
    let list = file.hosts.entry(host.to_owned()).or_default();
    list.retain(|existing| existing != directory);
    list.insert(0, directory.to_owned());
    list.truncate(RECENT_LIMIT);
    write(path, &file)
}

/// Forgets `directory` from `host`'s list.
pub fn remove(path: &Path, host: &str, directory: &Path) -> io::Result<()> {
    let mut file = read(path);
    let Some(list) = file.hosts.get_mut(host) else {
        return Ok(());
    };
    let before = list.len();
    list.retain(|existing| Path::new(existing) != directory);
    if list.len() == before {
        return Ok(());
    }
    write(path, &file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn most_recent_first_deduplicated_capped_and_per_host() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(FILE_NAME);
        let dirs: Vec<PathBuf> = (0..RECENT_LIMIT + 3)
            .map(|index| {
                let dir = temp.path().join(format!("d{index}"));
                fs::create_dir(&dir).unwrap();
                dir
            })
            .collect();
        for dir in &dirs {
            record(&path, LOCAL_HOST, dir).unwrap();
        }
        // Choosing an old one again moves it to the front, once.
        record(&path, LOCAL_HOST, &dirs[RECENT_LIMIT]).unwrap();
        let recents = load(&path, LOCAL_HOST);
        assert_eq!(recents.len(), RECENT_LIMIT);
        assert_eq!(recents[0], dirs[RECENT_LIMIT]);
        assert_eq!(recents[1], dirs[RECENT_LIMIT + 2]);
        assert!(!recents.contains(&dirs[0]));
        assert!(load(&path, "forge").is_empty());

        remove(&path, LOCAL_HOST, &dirs[RECENT_LIMIT]).unwrap();
        assert_eq!(load(&path, LOCAL_HOST)[0], dirs[RECENT_LIMIT + 2]);
    }

    #[test]
    fn deleted_directories_and_corrupt_files_read_as_nothing() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(FILE_NAME);
        let gone = temp.path().join("gone");
        fs::create_dir(&gone).unwrap();
        record(&path, LOCAL_HOST, &gone).unwrap();
        fs::remove_dir(&gone).unwrap();
        assert!(load(&path, LOCAL_HOST).is_empty());
        fs::write(&path, b"{not json").unwrap();
        assert!(load(&path, LOCAL_HOST).is_empty());
        // A corrupt file is replaced by the next choice, not an error.
        record(&path, LOCAL_HOST, temp.path()).unwrap();
        assert_eq!(load(&path, LOCAL_HOST), [temp.path().to_path_buf()]);
    }
}
