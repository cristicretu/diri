//! A private directory pinned across relative file operations.
#[cfg(windows)]
use std::path::PathBuf;
#[cfg(unix)]
use std::{
    ffi::CString,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::OpenOptionsExt,
    },
};
use std::{fs::File, io, path::Path};

pub struct PrivateDirectory {
    #[cfg(unix)]
    file: File,
    #[cfg(windows)]
    _ancestors: Vec<File>,
    #[cfg(windows)]
    path: PathBuf,
}
impl PrivateDirectory {
    pub fn open(path: &Path) -> io::Result<Self> {
        #[cfg(unix)]
        {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(path)?;
            use std::os::unix::fs::MetadataExt;
            let m = file.metadata()?;
            if !m.is_dir() || m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o077 != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "directory ownership or permissions are unsafe",
                ));
            }
            Ok(Self { file })
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
            use windows_sys::Win32::Storage::FileSystem::*;
            if !path.is_absolute() {
                return Err(io::ErrorKind::InvalidInput.into());
            }
            // Pin each ancestor without FILE_SHARE_DELETE: neither the private
            // directory nor a parent can be renamed/replaced under a path open.
            // Reject reparse points instead of traversing an attacker junction.
            let mut ancestors = Vec::new();
            let mut current = PathBuf::new();
            for component in path.components() {
                use std::path::Component;
                match component {
                    Component::Prefix(_) => {
                        current.push(component);
                        continue;
                    }
                    Component::RootDir | Component::Normal(_) => current.push(component),
                    _ => return Err(io::ErrorKind::InvalidInput.into()),
                }
                let file = std::fs::OpenOptions::new()
                    .read(true)
                    .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
                    .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
                    .open(&current)?;
                let m = file.metadata()?;
                if !m.is_dir() || m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                    return Err(io::Error::other(
                        "private directory ancestry contains a reparse point",
                    ));
                }
                ancestors.push(file);
            }
            crate::security::validate_directory(path, 0o077)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            Ok(Self {
                _ancestors: ancestors,
                path: path.into(),
            })
        }
    }
    fn name(name: &str) -> io::Result<()> {
        if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\', '\0', ':']) {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        Ok(())
    }
    pub fn create(&self, name: &str) -> io::Result<File> {
        self.open_file(name, true)
    }
    pub fn read(&self, name: &str) -> io::Result<File> {
        self.open_file(name, false)
    }
    fn open_file(&self, name: &str, create: bool) -> io::Result<File> {
        Self::name(name)?;
        #[cfg(unix)]
        {
            let name = CString::new(name)?;
            let flags = libc::O_NOFOLLOW
                | libc::O_CLOEXEC
                | if create {
                    libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL
                } else {
                    libc::O_RDONLY | libc::O_NONBLOCK
                };
            // SAFETY: live owned directory, validated single component, owned fd.
            let fd = unsafe { libc::openat(self.file.as_raw_fd(), name.as_ptr(), flags, 0o600) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(unsafe { File::from_raw_fd(fd) })
        }
        #[cfg(windows)]
        {
            crate::security::open_regular(&self.path.join(name), create, create, create)
        }
    }
    pub fn unlink(&self, name: &str) -> io::Result<()> {
        Self::name(name)?;
        #[cfg(unix)]
        {
            let name = CString::new(name)?;
            if unsafe { libc::unlinkat(self.file.as_raw_fd(), name.as_ptr(), 0) } != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
        #[cfg(windows)]
        {
            std::fs::remove_file(self.path.join(name))
        }
    }
    pub fn publish(&self, temporary: &str, destination: &str) -> io::Result<()> {
        Self::name(temporary)?;
        Self::name(destination)?;
        #[cfg(unix)]
        {
            let temporary = CString::new(temporary)?;
            let destination = CString::new(destination)?;
            if unsafe {
                libc::linkat(
                    self.file.as_raw_fd(),
                    temporary.as_ptr(),
                    self.file.as_raw_fd(),
                    destination.as_ptr(),
                    0,
                )
            } != 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
        #[cfg(windows)]
        {
            std::fs::hard_link(self.path.join(temporary), self.path.join(destination))
        }
    }
    pub fn sync(&self) -> io::Result<()> {
        #[cfg(unix)]
        {
            self.file.sync_all()
        }
        #[cfg(windows)]
        {
            crate::security::sync_directory(&self.path)
        }
    }
}
