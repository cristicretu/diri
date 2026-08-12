//! Local half of the bounded New Agent folder picker.

use std::collections::BinaryHeap;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use diri_proto::remote_pty::{
    DirectoryEntry, DirectoryEntryKind, DirectoryListMode, DirectoryListRequest,
    DirectoryListResult, MAX_DIRECTORY_ENTRIES, MAX_DIRECTORY_RESPONSE_BYTES,
    MAX_DIRECTORY_SCANNED_ENTRIES,
};

pub fn list(request: &DirectoryListRequest) -> io::Result<DirectoryListResult> {
    request
        .validate()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let canonical = fs::canonicalize(expand_home(&request.path)?)?;
    if !canonical.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "requested path is not a directory",
        ));
    }

    let mut entries =
        BinaryHeap::<(String, String, bool)>::with_capacity(MAX_DIRECTORY_ENTRIES + 1);
    let mut truncated = false;
    for (scanned, entry) in fs::read_dir(&canonical)?.enumerate() {
        if scanned == MAX_DIRECTORY_SCANNED_ENTRIES {
            truncated = true;
            break;
        }
        let entry = entry?;
        let file_type = entry.file_type()?;
        let is_directory = file_type.is_dir()
            || (file_type.is_symlink() && entry.metadata().is_ok_and(|metadata| metadata.is_dir()));
        let is_executable = request.mode == DirectoryListMode::Executables
            && !is_directory
            && entry.metadata().is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            });
        if !is_directory && !is_executable {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(ToOwned::to_owned) else {
            continue;
        };
        let path = canonical.join(&name).to_string_lossy().into_owned();
        if entries.len() < MAX_DIRECTORY_ENTRIES {
            entries.push((name, path, is_executable));
        } else {
            truncated = true;
            if entries
                .peek()
                .is_some_and(|(largest, _, _)| name.as_str() < largest.as_str())
            {
                entries.pop();
                entries.push((name, path, is_executable));
            }
        }
    }

    let path = canonical.to_string_lossy().into_owned();
    let parent = canonical
        .parent()
        .filter(|parent| *parent != canonical)
        .map(|parent| parent.to_string_lossy().into_owned());
    let fixed_bytes = serde_json::to_vec(&DirectoryListResult {
        path: path.clone(),
        parent: parent.clone(),
        entries: Vec::new(),
        truncated: true,
    })
    .map_err(io::Error::other)?
    .len();
    let mut response_bytes = fixed_bytes;
    let mut bounded = Vec::with_capacity(entries.len());
    for (name, path, is_executable) in entries.into_sorted_vec() {
        let entry = DirectoryEntry {
            name,
            path,
            kind: if is_executable {
                DirectoryEntryKind::Executable
            } else {
                DirectoryEntryKind::Directory
            },
        };
        let entry_bytes = serde_json::to_vec(&entry)
            .map_err(io::Error::other)?
            .len()
            .saturating_add(1);
        if response_bytes.saturating_add(entry_bytes) > MAX_DIRECTORY_RESPONSE_BYTES {
            truncated = true;
            break;
        }
        response_bytes += entry_bytes;
        bounded.push(entry);
    }

    Ok(DirectoryListResult {
        path,
        parent,
        entries: bounded,
        truncated,
    })
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
        Ok(Path::new(path).to_path_buf())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_listing_returns_only_sorted_directories() {
        let temp = tempfile::tempdir().expect("temp");
        fs::create_dir(temp.path().join("z")).expect("z");
        fs::create_dir(temp.path().join("a")).expect("a");
        fs::write(temp.path().join("file"), b"x").expect("file");
        let result = list(&DirectoryListRequest {
            path: temp.path().to_string_lossy().into_owned(),
            mode: DirectoryListMode::Directories,
        })
        .expect("list");
        assert_eq!(
            result
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "z"]
        );
    }
}
