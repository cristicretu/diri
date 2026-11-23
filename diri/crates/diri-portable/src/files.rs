//! Owner-only file writes and path validation shared by sync and migration.

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha256};

/// A relative `/`-separated path with no empty, `.`, `..` or absolute
/// component and no NUL or backslash: safe to join under a root.
pub fn safe_relative(rel: &str) -> bool {
    !rel.is_empty()
        && rel.len() <= 1024
        && !rel.starts_with('/')
        && !rel.contains('\\')
        && !rel.contains('\0')
        && rel
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

/// `root/rel` after [`safe_relative`], refusing a path whose existing
/// ancestors under `root` include a symlink (never follow one out of root).
pub fn confined(root: &Path, rel: &str) -> io::Result<PathBuf> {
    if !safe_relative(rel) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unsafe path {rel:?}"),
        ));
    }
    let path = root.join(rel);
    let mut current = root.to_path_buf();
    for component in Path::new(rel).components() {
        let Component::Normal(part) = component else {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "unsafe path"));
        };
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("refusing symlink {}", current.display()),
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => break,
            Err(error) => return Err(error),
        }
    }
    Ok(path)
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Creates `path`'s parent chain owner-only and replaces `path` atomically
/// (temporary sibling + rename), mode 0600.
pub fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    diri_platform::private::create_dir_all(parent)?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no name"))?
        .to_string_lossy();
    let temporary = parent.join(format!(".{name}.{}.diri-tmp", std::process::id()));
    diri_platform::private::replace_via(&temporary, path, bytes)
}

/// What [`write_if_absent`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteOutcome {
    Written,
    /// The same bytes were already there.
    Same,
    /// Different bytes were already there and were kept.
    KeptExisting,
}

/// Writes `bytes` at `path` only when nothing is there. Existing content is
/// never overwritten, so importing twice changes nothing the second time
/// (`writeIfAbsent_idempotent` in `formal/MigrationSync.lean`).
pub fn write_if_absent(path: &Path, bytes: &[u8], dry_run: bool) -> io::Result<WriteOutcome> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => {
            Ok(WriteOutcome::KeptExisting)
        }
        Ok(_) => {
            let existing = fs::read(path)?;
            Ok(if existing == bytes {
                WriteOutcome::Same
            } else {
                WriteOutcome::KeptExisting
            })
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if !dry_run {
                write_private(path, bytes)?;
            }
            Ok(WriteOutcome::Written)
        }
        Err(error) => Err(error),
    }
}

/// Reads at most `limit` bytes of a regular, non-symlink file.
pub fn read_bounded(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    use io::Read;
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    if meta.len() > limit {
        return Err(io::Error::new(
            io::ErrorKind::FileTooLarge,
            format!("{} is larger than {limit} bytes", path.display()),
        ));
    }
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    fs::File::open(path)?.take(limit).read_to_end(&mut bytes)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_paths_are_validated() {
        assert!(safe_relative("notes/a.md"));
        for bad in ["", "/a", "a/../b", "a//b", "./a", "a\\b", "a/."] {
            assert!(!safe_relative(bad), "{bad}");
        }
    }

    #[test]
    fn write_if_absent_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/b.txt");
        assert_eq!(
            write_if_absent(&path, b"one", false).unwrap(),
            WriteOutcome::Written
        );
        assert_eq!(
            write_if_absent(&path, b"one", false).unwrap(),
            WriteOutcome::Same
        );
        assert_eq!(
            write_if_absent(&path, b"two", false).unwrap(),
            WriteOutcome::KeptExisting
        );
        assert_eq!(fs::read(&path).unwrap(), b"one");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(path.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn confined_refuses_symlinked_ancestors() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("notes")).unwrap();
        assert!(confined(dir.path(), "notes/a.md").is_err());
        assert!(confined(dir.path(), "other/a.md").is_ok());
    }
}
