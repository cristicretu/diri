//! `list-dir-v1`: one bounded directory level, resolved the way a program in
//! the session would resolve its argument.
//!
//! The Helper process runs once per request, outside the Holder, so a slow
//! filesystem never touches the PTY loop and an idle session pays nothing.
//! Entry kinds come from the directory entry itself (no symlink following);
//! a symlink costs one `stat` to report its target's kind and is never
//! entered. Names, paths and counts never reach diagnostics.

use std::collections::BinaryHeap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Instant;

use diri_proto::remote_pty::{
    ListDirEntry, ListDirEntryKind, ListDirFailure, ListDirRequest, ListDirResult,
    MAX_LIST_DIR_RESPONSE_BYTES, MAX_LIST_DIR_SCANNED_ENTRIES,
};

/// Entries read between deadline checks.
const DEADLINE_STRIDE: usize = 256;

/// Join `requested` onto `cwd` like a shell argument: empty is `cwd`, `~`
/// and `~/…` are the account home, absolute paths stand alone. `.` and
/// empty components are dropped; `..` is kept for the kernel to resolve, as
/// it would for the program, rather than guessed lexically across symlinks.
pub(crate) fn resolve(cwd: &str, home: Option<&Path>, requested: &str) -> Option<PathBuf> {
    let joined = if requested.is_empty() {
        PathBuf::from(cwd)
    } else if requested == "~" || requested.starts_with("~/") {
        let mut path = home?.to_path_buf();
        if let Some(rest) = requested.strip_prefix("~/") {
            path.push(rest);
        }
        path
    } else if requested.starts_with('/') {
        PathBuf::from(requested)
    } else {
        Path::new(cwd).join(requested)
    };
    let mut clean = PathBuf::new();
    for component in joined.components() {
        match component {
            std::path::Component::CurDir => {}
            other => clean.push(other.as_os_str()),
        }
    }
    Some(clean)
}

/// List `request.path` resolved against the verified `cwd`. Filesystem
/// refusals are answers (`failure`), not RPC errors.
pub(crate) fn list_in(
    cwd: &str,
    home: Option<&Path>,
    request: &ListDirRequest,
    deadline: Instant,
) -> ListDirResult {
    let Some(path) = resolve(cwd, home, &request.path) else {
        return failed(cwd, request.path.clone(), ListDirFailure::NotFound);
    };
    let shown = path.to_string_lossy().into_owned();
    let read = match fs::read_dir(&path) {
        Ok(read) => read,
        Err(error) => return failed(cwd, shown, failure_for(&error)),
    };

    let limit = request.limit as usize;
    let prefix = request.prefix.to_lowercase();
    // A max-heap keeps the `limit` earliest names in O(limit) memory.
    let mut kept = BinaryHeap::<(String, ListDirEntryKind, bool)>::with_capacity(limit + 1);
    let mut truncated = false;
    for (scanned, entry) in read.enumerate() {
        if scanned == MAX_LIST_DIR_SCANNED_ENTRIES
            || (scanned % DEADLINE_STRIDE == 0 && scanned > 0 && Instant::now() >= deadline)
        {
            truncated = true;
            break;
        }
        let Ok(entry) = entry else {
            truncated = true;
            break;
        };
        let Ok(name) = entry.file_name().into_string() else {
            // The wire is UTF-8 and a lossy name could not be typed back.
            continue;
        };
        if !prefix.is_empty() && !name.to_lowercase().starts_with(&prefix) {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let symlink = file_type.is_symlink();
        let kind = if symlink {
            // One stat for the target's kind; the walk never enters it.
            match fs::metadata(entry.path()) {
                Ok(metadata) if metadata.is_dir() => ListDirEntryKind::Directory,
                Ok(metadata) if metadata.is_file() => ListDirEntryKind::File,
                _ => ListDirEntryKind::Other,
            }
        } else if file_type.is_dir() {
            ListDirEntryKind::Directory
        } else if file_type.is_file() {
            ListDirEntryKind::File
        } else {
            ListDirEntryKind::Other
        };
        if !request.include_files && kind != ListDirEntryKind::Directory {
            continue;
        }
        if kept.len() < limit {
            kept.push((name, kind, symlink));
        } else {
            truncated = true;
            if kept
                .peek()
                .is_some_and(|(largest, _, _)| name.as_str() < largest.as_str())
            {
                kept.pop();
                kept.push((name, kind, symlink));
            }
        }
    }

    let mut result = ListDirResult {
        cwd: cwd.to_owned(),
        path: shown,
        entries: Vec::new(),
        truncated,
        failure: None,
    };
    let mut bytes = serde_json::to_vec(&ListDirResult {
        truncated: true,
        ..result.clone()
    })
    .map_or(usize::MAX, |encoded| encoded.len());
    for (name, kind, symlink) in kept.into_sorted_vec() {
        let entry = ListDirEntry {
            name,
            kind,
            symlink,
        };
        let entry_bytes = serde_json::to_vec(&entry)
            .map_or(usize::MAX, |encoded| encoded.len())
            .saturating_add(1);
        if bytes.saturating_add(entry_bytes) > MAX_LIST_DIR_RESPONSE_BYTES {
            result.truncated = true;
            break;
        }
        bytes += entry_bytes;
        result.entries.push(entry);
    }
    result
}

fn failed(cwd: &str, path: String, failure: ListDirFailure) -> ListDirResult {
    ListDirResult {
        cwd: cwd.to_owned(),
        path,
        entries: Vec::new(),
        truncated: false,
        failure: Some(failure),
    }
}

fn failure_for(error: &io::Error) -> ListDirFailure {
    match error.kind() {
        io::ErrorKind::PermissionDenied => ListDirFailure::PermissionDenied,
        io::ErrorKind::NotFound => ListDirFailure::NotFound,
        io::ErrorKind::NotADirectory => ListDirFailure::NotADirectory,
        _ => ListDirFailure::Unreadable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diri_proto::remote_pty::{MAX_LIST_DIR_ENTRIES, SessionSelector, SessionToken};
    use std::os::unix::fs::PermissionsExt as _;
    use std::time::Duration;

    fn request(path: &str, include_files: bool) -> ListDirRequest {
        ListDirRequest {
            selector: SessionSelector {
                session_id: "s".into(),
                session_token: SessionToken::new("0123456789abcdef0123").unwrap(),
                expected_incarnation: Some("i".into()),
            },
            path: path.into(),
            include_files,
            prefix: String::new(),
            limit: MAX_LIST_DIR_ENTRIES as u32,
            timeout_ms: 1000,
        }
    }

    fn later() -> Instant {
        Instant::now() + Duration::from_secs(5)
    }

    fn names(result: &ListDirResult) -> Vec<&str> {
        result
            .entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect()
    }

    #[test]
    fn paths_resolve_like_a_shell_argument() {
        let home = Path::new("/home/me");
        let cwd = "/work/repo";
        let resolve = |path| resolve(cwd, Some(home), path).unwrap();
        assert_eq!(resolve(""), Path::new("/work/repo"));
        assert_eq!(resolve("src/./lib"), Path::new("/work/repo/src/lib"));
        assert_eq!(resolve("../other"), Path::new("/work/repo/../other"));
        assert_eq!(resolve("~"), Path::new("/home/me"));
        assert_eq!(resolve("~/code"), Path::new("/home/me/code"));
        assert_eq!(resolve("/etc"), Path::new("/etc"));
        // `~user` is not this account's home; it is a relative name.
        assert_eq!(resolve("~other"), Path::new("/work/repo/~other"));
        assert_eq!(super::resolve(cwd, None, "~/x"), None);
    }

    #[test]
    fn lists_directories_unless_files_are_asked_for() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("src")).unwrap();
        fs::create_dir(temp.path().join("docs")).unwrap();
        fs::write(temp.path().join("README.md"), b"x").unwrap();
        let cwd = temp.path().to_str().unwrap();

        let dirs = list_in(cwd, None, &request("", false), later());
        assert_eq!(names(&dirs), ["docs", "src"]);
        assert_eq!(dirs.cwd, cwd);
        assert!(!dirs.truncated && dirs.failure.is_none());

        let all = list_in(cwd, None, &request(".", true), later());
        assert_eq!(names(&all), ["README.md", "docs", "src"]);
        assert_eq!(all.entries[0].kind, ListDirEntryKind::File);
    }

    #[test]
    fn relative_paths_and_prefixes_use_the_child_cwd() {
        let temp = tempfile::tempdir().unwrap();
        let src = temp.path().join("src");
        fs::create_dir(&src).unwrap();
        for name in ["Render.rs", "render_tests.rs", "main.rs"] {
            fs::write(src.join(name), b"").unwrap();
        }
        let mut wanted = request("src", true);
        wanted.prefix = "rEnD".into();
        let result = list_in(temp.path().to_str().unwrap(), None, &wanted, later());
        assert_eq!(names(&result), ["Render.rs", "render_tests.rs"]);
        assert_eq!(result.path, src.to_str().unwrap());
    }

    #[test]
    fn symlinks_report_their_target_kind_and_are_never_entered() {
        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::create_dir(outside.path().join("deep")).unwrap();
        std::os::unix::fs::symlink(outside.path(), temp.path().join("linked")).unwrap();
        std::os::unix::fs::symlink("/nonexistent/target", temp.path().join("dangling")).unwrap();
        let result = list_in(
            temp.path().to_str().unwrap(),
            None,
            &request("", true),
            later(),
        );
        assert_eq!(
            result.entries,
            vec![
                ListDirEntry {
                    name: "dangling".into(),
                    kind: ListDirEntryKind::Other,
                    symlink: true,
                },
                ListDirEntry {
                    name: "linked".into(),
                    kind: ListDirEntryKind::Directory,
                    symlink: true,
                },
            ]
        );
    }

    #[test]
    fn filesystem_refusals_are_structured_answers() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().to_str().unwrap();
        fs::write(temp.path().join("file"), b"").unwrap();
        let locked = temp.path().join("locked");
        fs::create_dir(&locked).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

        let missing = list_in(cwd, None, &request("missing", true), later());
        assert_eq!(missing.failure, Some(ListDirFailure::NotFound));
        let file = list_in(cwd, None, &request("file", true), later());
        assert_eq!(file.failure, Some(ListDirFailure::NotADirectory));
        // Root ignores permission bits; only an ordinary account sees EACCES.
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } != 0 {
            let denied = list_in(cwd, None, &request("locked", true), later());
            assert_eq!(denied.failure, Some(ListDirFailure::PermissionDenied));
            assert_eq!(denied.path, locked.to_str().unwrap());
        }
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn large_directories_are_bounded_to_the_earliest_names() {
        let temp = tempfile::tempdir().unwrap();
        for index in 0..=MAX_LIST_DIR_ENTRIES {
            fs::write(temp.path().join(format!("f{index:05}")), b"").unwrap();
        }
        let result = list_in(
            temp.path().to_str().unwrap(),
            None,
            &request("", true),
            later(),
        );
        assert_eq!(result.entries.len(), MAX_LIST_DIR_ENTRIES);
        assert!(result.truncated);
        assert_eq!(result.entries[0].name, "f00000");
        assert_eq!(
            result.entries.last().unwrap().name,
            format!("f{:05}", MAX_LIST_DIR_ENTRIES - 1)
        );

        let mut small = request("", true);
        small.limit = 3;
        let result = list_in(temp.path().to_str().unwrap(), None, &small, later());
        assert_eq!(names(&result), ["f00000", "f00001", "f00002"]);
        assert!(result.truncated);
    }

    #[test]
    fn an_expired_deadline_truncates_instead_of_running_on() {
        let temp = tempfile::tempdir().unwrap();
        for index in 0..(DEADLINE_STRIDE * 2) {
            fs::write(temp.path().join(format!("f{index:05}")), b"").unwrap();
        }
        let result = list_in(
            temp.path().to_str().unwrap(),
            None,
            &request("", true),
            Instant::now(),
        );
        assert!(result.truncated);
        assert_eq!(result.entries.len(), DEADLINE_STRIDE);
    }
}
