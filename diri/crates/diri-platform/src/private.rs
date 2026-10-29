//! Owner-only creation for everything Diri writes under its own state,
//! runtime, log, cache, config and data directories.
//!
//! Files are created `0600` and directories `0700` by the creating call
//! itself, so there is no window in which a fresh file is readable by other
//! users (a chmod after create leaves one). `mode` only applies to a file the
//! call creates, so a rewrite of an older file also drops group/other bits on
//! the open descriptor before the first byte lands.
//!
//! Windows keeps its existing behavior: the per-user profile directories are
//! already private, and DACL-protected paths go through [`crate::security`].

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

/// Permission bits that must never be set on Diri's private state.
pub const SHARED_BITS: u32 = 0o077;

/// Creates `path` and every missing ancestor `0700`. Directories that already
/// exist keep their mode; [`tighten_tree`] repairs older ones once at startup.
pub fn create_dir_all(path: &Path) -> io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(path)
}

/// Write options whose create is owner-only (`0600`). Callers add the access
/// flags (`write`, `append`, `create`, `create_new`, `truncate`).
pub fn options() -> OpenOptions {
    #[allow(unused_mut)]
    let mut options = OpenOptions::new();
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options
}

/// Drops group/other bits from an open file that predates owner-only
/// creation. No syscall beyond one `fstat` when it is already private.
pub fn restrict(file: &File) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = file.metadata()?.permissions().mode();
        if mode & SHARED_BITS != 0 {
            file.set_permissions(std::fs::Permissions::from_mode(mode & 0o7700))?;
        }
    }
    #[cfg(not(unix))]
    let _ = file;
    Ok(())
}

/// Opens `path` for writing, created `0600` and truncated; an existing
/// world-readable file is made owner-only before anything is written.
pub fn create(path: &Path) -> io::Result<File> {
    let file = options()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    restrict(&file)?;
    Ok(file)
}

/// Opens `path` for appending, created `0600`; an existing world-readable
/// file is made owner-only before anything is appended.
pub fn append(path: &Path) -> io::Result<File> {
    let file = options().append(true).create(true).open(path)?;
    restrict(&file)?;
    Ok(file)
}

/// [`std::fs::write`], owner-only.
pub fn write(path: &Path, contents: impl AsRef<[u8]>) -> io::Result<()> {
    create(path)?.write_all(contents.as_ref())
}

/// Writes `contents` to `temporary` owner-only, then renames it over `path`.
/// The temporary is removed again when any step fails.
pub fn replace_via(temporary: &Path, path: &Path, contents: impl AsRef<[u8]>) -> io::Result<()> {
    let result = write(temporary, contents).and_then(|()| std::fs::rename(temporary, path));
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}

/// [`replace_via`] for contents produced by `write` into a buffered stream on
/// the owner-only temporary, so a large file is never staged in memory.
pub fn replace_streamed(
    temporary: &Path,
    path: &Path,
    write: impl FnOnce(&mut io::BufWriter<File>) -> io::Result<()>,
) -> io::Result<()> {
    let result = create(temporary)
        .and_then(|file| {
            let mut stream = io::BufWriter::new(file);
            write(&mut stream)?;
            stream
                .into_inner()
                .map_err(io::IntoInnerError::into_error)?;
            Ok(())
        })
        .and_then(|()| std::fs::rename(temporary, path));
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}

/// What one [`tighten_tree`] pass saw and changed.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Tightened {
    pub visited: usize,
    pub changed: usize,
}

/// Removes group/other permission bits from `root` and everything beneath it
/// that the current user owns, so state written by older builds (or by a
/// writer that relied on the umask) becomes owner-only. Owner bits, including
/// execute, are kept. Symlinks are never followed or changed, and the walk
/// stops after `max_entries` so a pathological tree cannot stall startup.
pub fn tighten_tree(root: &Path, max_entries: usize) -> Tightened {
    #[allow(unused_mut)]
    let mut report = Tightened::default();
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        // SAFETY: geteuid has no preconditions.
        let uid = unsafe { libc::geteuid() };
        let mut stack = vec![root.to_path_buf()];
        while let Some(path) = stack.pop() {
            if report.visited >= max_entries {
                break;
            }
            let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            report.visited += 1;
            if metadata.file_type().is_symlink() || metadata.uid() != uid {
                continue;
            }
            let mode = metadata.permissions().mode();
            if mode & SHARED_BITS != 0
                && std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode & 0o7700))
                    .is_ok()
            {
                report.changed += 1;
            }
            if metadata.is_dir()
                && let Ok(entries) = std::fs::read_dir(&path)
            {
                stack.extend(entries.flatten().map(|entry| entry.path()));
            }
        }
    }
    #[cfg(not(unix))]
    let _ = (root, max_entries);
    report
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode(path: &Path) -> u32 {
        std::fs::symlink_metadata(path)
            .unwrap()
            .permissions()
            .mode()
            & 0o7777
    }

    /// Runs `body` under a permissive umask, as a login shell or launchd
    /// commonly provides, so the assertions prove the writer and not the
    /// test runner's umask. The umask is process-wide, hence one lock.
    fn with_open_umask<T>(body: impl FnOnce() -> T) -> T {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
        // SAFETY: umask has no preconditions; restored before returning.
        let previous = unsafe { libc::umask(0o022) };
        let result = body();
        unsafe { libc::umask(previous) };
        result
    }

    #[test]
    fn a_streamed_replacement_is_owner_only_and_leaves_nothing_on_failure() {
        with_open_umask(|| {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("screen.plist");
            let tmp = temp.path().join("screen.plist.tmp");
            write(&path, b"old").unwrap();
            replace_streamed(&tmp, &path, |stream| stream.write_all(b"new")).unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), b"new");
            assert_eq!(mode(&path), 0o600);
            assert!(!tmp.exists());

            let failed = replace_streamed(&tmp, &path, |stream| {
                stream.write_all(b"partial")?;
                Err(io::Error::other("encode failed"))
            });
            assert!(failed.is_err());
            assert_eq!(std::fs::read(&path).unwrap(), b"new", "kept on failure");
            assert!(!tmp.exists(), "temporary removed");
        });
    }

    #[test]
    fn new_files_and_directories_are_owner_only_from_creation() {
        with_open_umask(|| {
            let temp = tempfile::tempdir().unwrap();
            let nested = temp.path().join("a/b/c");
            create_dir_all(&nested).unwrap();
            for dir in ["a", "a/b", "a/b/c"] {
                assert_eq!(mode(&temp.path().join(dir)), 0o700, "{dir}");
            }
            let file = nested.join("state.json");
            write(&file, b"{}").unwrap();
            assert_eq!(mode(&file), 0o600);
            let appended = nested.join("boot.log");
            append(&appended).unwrap().write_all(b"x").unwrap();
            assert_eq!(mode(&appended), 0o600);
            let replaced = nested.join("checkpoint.plist");
            replace_via(&nested.join("checkpoint.plist.tmp"), &replaced, b"x").unwrap();
            assert_eq!(mode(&replaced), 0o600);
            assert!(!nested.join("checkpoint.plist.tmp").exists());
        });
    }

    #[test]
    fn rewriting_a_world_readable_file_makes_it_owner_only() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("prefs.json");
        std::fs::write(&file, b"old").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        write(&file, b"new").unwrap();
        assert_eq!(mode(&file), 0o600);
        assert_eq!(std::fs::read(&file).unwrap(), b"new");

        let log = temp.path().join("boot.log");
        std::fs::write(&log, b"a").unwrap();
        std::fs::set_permissions(&log, std::fs::Permissions::from_mode(0o664)).unwrap();
        append(&log).unwrap().write_all(b"b").unwrap();
        assert_eq!(mode(&log), 0o600);
        assert_eq!(std::fs::read(&log).unwrap(), b"ab");
    }

    #[test]
    fn tightening_keeps_owner_bits_and_never_follows_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("state");
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(root.join("logs")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(root.join("logs/s_1.screen.plist"), b"x").unwrap();
        std::fs::write(root.join("dirijor"), b"#!").unwrap();
        std::fs::write(outside.join("mine.txt"), b"x").unwrap();
        for (path, bits) in [
            (root.clone(), 0o755),
            (root.join("logs"), 0o755),
            (root.join("logs/s_1.screen.plist"), 0o644),
            (root.join("dirijor"), 0o755),
            (outside.clone(), 0o755),
            (outside.join("mine.txt"), 0o644),
        ] {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(bits)).unwrap();
        }
        std::os::unix::fs::symlink(&outside, root.join("linked")).unwrap();

        let report = tighten_tree(&root, 1_000);
        assert_eq!(report.changed, 4);
        assert_eq!(mode(&root), 0o700);
        assert_eq!(mode(&root.join("logs")), 0o700);
        assert_eq!(mode(&root.join("logs/s_1.screen.plist")), 0o600);
        assert_eq!(mode(&root.join("dirijor")), 0o700);
        assert_eq!(
            mode(&outside),
            0o755,
            "symlink target is not ours to change"
        );
        assert_eq!(mode(&outside.join("mine.txt")), 0o644);

        assert_eq!(
            tighten_tree(&root, 1_000).changed,
            0,
            "second pass is a no-op"
        );
        assert_eq!(tighten_tree(&root, 2).visited, 2, "the walk is bounded");
    }
}
