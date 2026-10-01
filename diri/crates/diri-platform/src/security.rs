//! One owner-only file policy. Windows uses explicit, protected user-SID DACLs.
#[cfg(unix)]
use std::{io, path::Path};

#[cfg(unix)]
pub fn private_dir_all(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)?;
    if std::fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(io::Error::other("private directory is a symlink"));
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

#[cfg(unix)]
pub fn owner_only(path: &Path, directory: bool) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(
        path,
        std::fs::Permissions::from_mode(if directory { 0o700 } else { 0o600 }),
    )
}

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{current_user_sid, owner_only, private_dir_all};

/// Open a regular, caller-owned file without following its final symlink.
pub fn read_owned(path: &std::path::Path, private: bool) -> std::io::Result<std::fs::File> {
    let file = open_regular(path, false, false, false)?;
    validate_file(&file, private)?;
    Ok(file)
}

pub fn create_private(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    open_regular(path, true, true, true)
}

pub fn open_private_rw(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    let file = open_regular(path, true, true, false)?;
    validate_file(&file, false)?;
    owner_only(path, false)?;
    Ok(file)
}

pub fn open_regular(
    path: &std::path::Path,
    write: bool,
    create: bool,
    exclusive: bool,
) -> std::io::Result<std::fs::File> {
    #[cfg(unix)]
    let file = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .write(write)
            .create(create)
            .create_new(exclusive)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)?
    };
    #[cfg(windows)]
    let file = windows::open_file(path, write, create, exclusive)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other("expected a regular file"));
    }
    Ok(file)
}

pub fn validate_file(file: &std::fs::File, private: bool) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let m = file.metadata()?;
        if !m.is_file()
            || m.nlink() != 1
            || m.uid() != unsafe { libc::geteuid() }
            || (private && m.mode() & 0o077 != 0)
        {
            return Err(std::io::Error::other(
                "file ownership or permissions are unsafe",
            ));
        }
        Ok(())
    }
    #[cfg(windows)]
    windows::validate_file(file, private)
}

pub fn validate_directory(path: &std::path::Path, forbidden_mode: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::symlink_metadata(path)?;
        if !m.is_dir() || m.uid() != unsafe { libc::geteuid() } || m.mode() & forbidden_mode != 0 {
            return Err(std::io::Error::other(
                "directory ownership or permissions are unsafe",
            ));
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        let _ = forbidden_mode;
        windows::validate_directory(path)
    }
}

pub fn sync_directory(path: &std::path::Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::fs::File::open(path)?.sync_all()
    }
    #[cfg(windows)]
    {
        // Windows has no supported unprivileged directory fsync. File writers
        // flush their file handles before atomic replace; do not invent stronger
        // power-loss durability or request elevated directory handles.
        validate_directory(path, 0)
    }
}
