//! Finding — and judging — the `.app` this process is running out of.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{Result, UpdateError};
use crate::version::Version;

/// Walks `…/diri.app/Contents/MacOS/diri` back up to `…/diri.app`.
///
/// Returns `None` for a bare `cargo run` binary, which is the signal that this
/// build has nothing to update.
pub fn enclosing_bundle(executable: &Path) -> Option<PathBuf> {
    let macos = executable.parent()?;
    let contents = macos.parent()?;
    let app = contents.parent()?;
    (macos.file_name()? == "MacOS"
        && contents.file_name()? == "Contents"
        && app.extension()? == "app")
        .then(|| app.to_path_buf())
}

pub fn running_bundle() -> Option<PathBuf> {
    let executable = std::env::current_exe().ok()?;
    // Resolve symlinks first: a bundle reached through one would otherwise be
    // swapped at the link's location rather than the real app's.
    let executable = executable.canonicalize().unwrap_or(executable);
    enclosing_bundle(&executable)
}

/// macOS version of this machine, for the feed's `minimum_system_version`.
///
/// Falls back to `0.0.0` when `sw_vers` is unavailable, which makes every
/// release with a floor ineligible — the safe direction to fail.
pub fn system_version() -> Version {
    Command::new("/usr/bin/sw_vers")
        .arg("-productVersion")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| Version::parse(String::from_utf8_lossy(&output.stdout).trim()))
        .unwrap_or_default()
}

/// The swap renames the bundle aside and unpacks its replacement next to it,
/// so the permission that matters is write on the *containing directory*, not
/// on the bundle. An app in `/Applications` on a machine where the user is not
/// an admin fails here, before anything has been downloaded.
pub fn ensure_writable(bundle: &Path) -> Result<()> {
    let parent = bundle
        .parent()
        .ok_or_else(|| UpdateError::NotWritable(format!("{} has no parent", bundle.display())))?;
    if directory_is_writable(parent) {
        return Ok(());
    }
    Err(UpdateError::NotWritable(format!(
        "{} is not writable by this user",
        parent.display()
    )))
}

/// Probes by creating a file rather than reading the mode bits: ownership,
/// ACLs, and a read-only mount all make a `drwxr-xr-x` directory unwritable,
/// and only the attempt distinguishes them.
fn directory_is_writable(directory: &Path) -> bool {
    let probe = directory.join(format!(".diri-update-probe-{}", std::process::id()));
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_app_above_a_bundled_executable() {
        let bundle = enclosing_bundle(Path::new("/Applications/diri.app/Contents/MacOS/diri"));
        assert_eq!(bundle, Some(PathBuf::from("/Applications/diri.app")));
    }

    #[test]
    fn a_bare_binary_has_no_bundle() {
        assert!(
            enclosing_bundle(Path::new("/Users/giga/fun/dirijor/diri/target/debug/diri")).is_none()
        );
        assert!(enclosing_bundle(Path::new("/usr/local/bin/diri")).is_none());
        // Right depth, wrong layout.
        assert!(enclosing_bundle(Path::new("/opt/diri/Contents/MacOS/diri")).is_none());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn reports_a_plausible_system_version() {
        // Guards the parse, not the value: any real macOS is well past 10.
        assert!(system_version() >= Version::new(10, 0, 0));
    }

    #[test]
    fn a_temp_directory_is_writable_and_a_system_one_is_not() {
        let directory = tempfile::tempdir().expect("temp dir");
        let bundle = directory.path().join("diri.app");
        std::fs::create_dir(&bundle).expect("create bundle");
        assert!(ensure_writable(&bundle).is_ok());
        assert!(ensure_writable(Path::new("/usr/lib/dyld")).is_err());
    }
}
